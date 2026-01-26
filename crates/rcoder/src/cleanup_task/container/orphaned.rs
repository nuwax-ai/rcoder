//! 孤立容器清理器
//!
//! 清理 DuckDB 中没有对应记录的孤立容器

use crate::cleanup_task::config::CleanupConfig;
use crate::cleanup_task::strategies::DestroyReason;
use anyhow::Result;
use chrono::{DateTime, Utc};
use shared_types::ServiceType;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// 孤立容器信息
#[derive(Debug, Clone)]
struct OrphanedContainerInfo {
    /// 标识符（project_id 或 user_id）
    id: String,
    /// 容器名称
    container_name: String,
    /// 服务类型
    service_type: ServiceType,
    /// 容器创建时间
    created_at: Option<DateTime<Utc>>,
    /// 销毁原因
    reason: DestroyReason,
}

/// 孤立容器清理器
pub struct OrphanedContainerCleaner {
    pub docker_manager: Arc<docker_manager::DockerManager>,
    pub state: Arc<crate::router::AppState>,
    pub container_patterns: Vec<String>,
    /// 清理配置
    pub config: Arc<CleanupConfig>,
}

impl OrphanedContainerCleaner {
    pub fn new(
        docker_manager: Arc<docker_manager::DockerManager>,
        state: Arc<crate::router::AppState>,
        container_patterns: Vec<String>,
        config: Arc<CleanupConfig>,
    ) -> Self {
        Self {
            docker_manager,
            state,
            container_patterns,
            config,
        }
    }

    /// 清理孤立容器
    /// 注意：无清理数量上限，一次性清理所有孤立容器
    /// 保护机制：120秒总超时 + 异步执行
    pub async fn cleanup(&self) -> Result<u64> {
        info!("🔍 [orphaned] 开始检查孤立容器");

        let total_timeout = Duration::from_secs(120);
        let cleaned_count = timeout(total_timeout, self.cleanup_inner()).await??;

        info!("✅ [orphaned] 孤立容器清理完成: {} 个", cleaned_count);
        Ok(cleaned_count)
    }

    async fn cleanup_inner(&self) -> Result<u64> {
        if self.container_patterns.is_empty() {
            warn!("⚠️ [orphaned] 没有启用的服务，跳过孤立容器清理");
            return Ok(0);
        }

        // 🔧 清理所有发现的孤立容器（无上限）
        let mut total_cleaned = 0;

        for pattern in &self.container_patterns {
            let service_type = match Self::infer_service_type_from_pattern(pattern) {
                Some(st) => st,
                None => {
                    warn!("⚠️ [orphaned] 无法识别的容器模式: {}", pattern);
                    continue;
                }
            };

            let containers = match self
                .docker_manager
                .list_containers_with_pattern(pattern)
                .await
            {
                Ok(containers) => containers,
                Err(e) => {
                    warn!("列出容器失败（模式: {}）: {}", pattern, e);
                    continue;
                }
            };

            let orphaned_containers: Vec<OrphanedContainerInfo> = containers
                .iter()
                .filter_map(|container| {
                    if let Some(names) = &container.names {
                        for name in names {
                            let clean_name = name.trim_start_matches('/');
                            if let Some(id) = Self::extract_id_from_container_name(clean_name) {
                                // 🔧 根据 service_type 使用不同的查询方式
                                // 🛡️ 关键修复：只检查 DuckDB 存储来判断是否为孤立容器
                                // 不再依赖 DockerManager 的内存缓存，因为缓存可能不会及时清理
                                let duckdb_has_record = match service_type {
                                    ServiceType::RCoder => {
                                        // RCoder: id 是 project_id，直接查询
                                        self.state.projects.contains_key(&id)
                                    }
                                    ServiceType::ComputerAgentRunner => {
                                        // ComputerAgentRunner: id 是 user_id，检查是否有任何项目使用该 user_id
                                        !self.state.projects.find_projects_by_user_id(&id).is_empty()
                                    }
                                };

                                // ✅ 只依赖 DuckDB 判断：DuckDB 中没有记录 = 孤立容器
                                // Docker 容器存在 + DuckDB 无记录 = 孤立容器，需要清理
                                let is_orphaned = !duckdb_has_record;

                                if is_orphaned {
                                    // 🔧 修复时间戳解析：Docker API 返回的 created 是 Unix 秒时间戳（不是毫秒）
                                    let created_time = container.created.and_then(|ts| {
                                        debug!(
                                            "🕐 [orphaned] 容器原始时间戳(秒): {}, container={}",
                                            ts, clean_name
                                        );
                                        // created 是秒级时间戳，直接使用
                                        DateTime::from_timestamp(ts, 0)
                                    });

                                    // 🛡️ 保护期检查：刚启动的容器不应该被清理
                                    if let Some(created) = created_time {
                                        let age = Utc::now().signed_duration_since(created);
                                        let age_seconds = age.num_seconds();
                                        let protection_seconds = self.config.container_protection_duration.as_secs() as i64;

                                        debug!(
                                            "🕐 [orphaned] 容器年龄检查: {}, age={}秒, protection={}秒",
                                            clean_name, age_seconds, protection_seconds
                                        );

                                        if age_seconds < protection_seconds {
                                            info!(
                                                "🛡️ [orphaned] 容器在保护期内，跳过清理: {}, 创建时长={}秒",
                                                clean_name,
                                                age_seconds
                                            );
                                            continue; // 跳过此容器，不清理
                                        }
                                    } else {
                                        warn!(
                                            "⚠️ [orphaned] 容器时间戳解析失败: {}, created={:?}",
                                            clean_name, container.created
                                        );
                                        // 时间戳解析失败，跳过此容器以避免误删
                                        continue;
                                    }

                                    return Some(OrphanedContainerInfo {
                                        id,
                                        container_name: clean_name.to_string(),
                                        service_type: service_type.clone(),
                                        created_at: created_time,
                                        reason: DestroyReason::Orphaned {
                                            created_at: created_time.unwrap_or_else(|| Utc::now()),
                                            was_protected: false,
                                        },
                                    });
                                }
                            }
                        }
                    }
                    None
                })
                .collect();

            if orphaned_containers.is_empty() {
                continue;
            }

            // 🔧 清理所有发现的孤立容器（无上限）
            for info in orphaned_containers {
                if self.cleanup_single(&info).await.is_ok() {
                    total_cleaned += 1;
                }
            }
        }

        Ok(total_cleaned)
    }

    async fn cleanup_single(&self, info: &OrphanedContainerInfo) -> Result<()> {
        info!(
            "🔥 [orphaned] 开始清理孤立容器: {} (id={}, type={:?}, 原因={})",
            info.container_name,
            info.id,
            info.service_type,
            info.reason.as_str()
        );
        debug!("📋 [orphaned] 销毁详情: {}", info.reason.description());

        // 🛡️ 二次保护期检查：在实际销毁前再次确认容器是否存在且在保护期内
        // 这是为了防止从收集孤立容器列表到实际销毁之间的时间差
        // 🔍 使用 find_container_realtime 获取最新的容器 ID
        let (container_info, cache_key) = match self
            .docker_manager
            .find_container_realtime(&info.container_name)
            .await
        {
            Ok(Some(result)) => {
                // 容器存在，检查保护期
                // 🔧 关键修复：使用 DockerManager 封装的方法获取创建时间
                // 使用容器 name 查询，容器重启后 name 不变，但 ID 会变
                match self
                    .docker_manager
                    .get_container_creation_time_by_name(&info.container_name)
                    .await
                {
                    Ok(Some(created_time_utc)) => {
                        let age = Utc::now().signed_duration_since(created_time_utc);
                        let protection_seconds =
                            self.config.container_protection_duration.as_secs() as i64;

                        if age.num_seconds() < protection_seconds {
                            info!(
                                "🛡️ [orphaned] 二次检查：容器在保护期内，跳过销毁: {}, age={}秒, protection={}秒",
                                info.container_name,
                                age.num_seconds(),
                                protection_seconds
                            );
                            return Ok(());
                        }
                    }
                    Ok(None) => {
                        // 容器不存在或创建时间为空
                        info!(
                            "⚠️ [orphaned] 容器不存在或创建时间为空，可能已被删除: name={}",
                            info.container_name
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            "⚠️ [orphaned] 获取容器创建时间失败: {}, error={}",
                            info.container_name, e
                        );
                        // 获取时间失败时，为了安全起见，跳过清理
                        return Ok(());
                    }
                }
                // 使用 info.id (project_id) 作为缓存 key
                (result, info.id.clone())
            }
            Ok(None) => {
                // 容器已不存在，可能已被删除
                info!(
                    "⚠️ [orphaned] 容器不存在，可能已被删除: name={}",
                    info.container_name
                );
                return Ok(());
            }
            Err(e) => {
                // 查询出错
                return Err(anyhow::anyhow!(
                    "查询容器信息失败: name={}, error={}",
                    info.container_name,
                    e
                ));
            }
        };

        // 🛡️ 活跃状态检查 (Active Safety Net)
        // 防止误杀正在活跃但 DB 记录丢失的容器
        // 如果容器还能响应 gRPC 且 report Active，则绝对不能杀
        // 🔧 通过容器名称重新获取完整的容器信息（包含 IP）
        let container_ip: Option<String> = match self.docker_manager.get_agent_info(&info.id).await {
            Ok(Some(agent_info)) => Some(agent_info.container_ip),
            Ok(None) => {
                // 无法获取容器信息，可能容器已经不存在了，允许清理
                debug!(
                    "⚠️ [orphaned] 无法获取容器 IP 信息，视为可清理: name={}",
                    info.container_name
                );
                None // 继续执行清理（没有 IP 也无法做 gRPC 检查）
            }
            Err(e) => {
                // 获取容器信息失败，可能是容器已经不存在，允许清理
                debug!(
                    "⚠️ [orphaned] 获取容器 IP 失败，视为可清理: name={}, error={}",
                    info.container_name, e
                );
                None // 继续执行清理
            }
        };

        // 如果成功获取到 IP，进行 gRPC 健康检查
        if let Some(ip) = container_ip {
            let grpc_addr = format!(
                "{}:{}",
                ip,
                shared_types::GRPC_DEFAULT_PORT
            );

            let status_checker =
                crate::cleanup_task::agent::AgentStatusChecker::new(self.state.grpc_pool.clone());

            // 构造查询 ID (对于 orphaned 容器，我们只能使用 info.id 尽力尝试)
            // RCoder: info.id = project_id
            // Computer: info.id = user_id
            let (user_id, project_id) = match info.service_type {
                ServiceType::RCoder => (info.id.clone(), info.id.clone()),
                ServiceType::ComputerAgentRunner => (info.id.clone(), info.id.clone()),
            };

            match status_checker
                .is_container_active(&grpc_addr, &user_id, &project_id)
                .await
            {
                Ok(true) => {
                    warn!(
                        "🚨 [orphaned] 阻止误杀：容器虽被标记为孤立（DB无记录），但在 gRPC 检查中处于活跃状态！跳过清理。name={}, ip={}",
                        info.container_name, ip
                    );
                    // 可以在这里触发警报，因为 DB 和容器状态不一致
                    return Ok(());
                }
                Ok(false) => {
                    debug!(
                        "✅ [orphaned] 容器确认非活跃，可以清理: {}",
                        info.container_name
                    );
                }
                Err(e) => {
                    // 连接失败或超时，说明容器可能已经hung住或死掉，允许清理
                    debug!("⚠️ [orphaned] 容器 gRPC 检查失败，视为可清理: error={}", e);
                }
            }
        }

        // 使用最新的 container_id 执行销毁
        docker_manager::container_stop::runtime_cleanup_container(
            &self.docker_manager,
            &container_info.container_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("清理容器失败: {}", e))?;

        // 清理 DockerManager 内存缓存（防止缓存残留）
        // 🔧 对于 RCoder: cache_key = project_id; 对于 ComputerAgentRunner: 可能是 user_id 或 container_id
        // 如果直接删除失败，尝试通过容器名称遍历查找并删除缓存
        let mut removed = false;
        if let Some(_) = self.docker_manager.remove_container_cache(&cache_key).await {
            debug!(
                "🧹 [orphaned] 已通过 identifier 清理 DockerManager 内存缓存: {}",
                cache_key
            );
            removed = true;
        } else {
            // 回退方案：遍历缓存，通过容器名称匹配删除
            // 这是为了处理 ComputerAgentRunner 容器缓存可能使用 container_id 作为 key 的情况
            for container_entry in self.docker_manager.list_containers().await {
                if container_entry.container_name == info.container_name {
                    if let Some(_) = self
                        .docker_manager
                        .remove_container_cache(&container_entry.project_id)
                        .await
                    {
                        debug!(
                            "🧹 [orphaned] 已通过容器名称清理 DockerManager 内存缓存: name={}, project_id={}",
                            info.container_name, container_entry.project_id
                        );
                        removed = true;
                        break;
                    }
                }
            }
        }

        if !removed {
            debug!(
                "⚠️ [orphaned] DockerManager 内存缓存未找到或已清理: name={}, cache_key={}",
                info.container_name, cache_key
            );
        }

        // 对于 ComputerAgentRunner，清理 VNC 后端
        if info.service_type == ServiceType::ComputerAgentRunner {
            if let Some(ref pingora_service) = self.state.pingora_service {
                match pingora_service.remove_vnc_backend(&info.id) {
                    Some(removed) => {
                        info!("✅ [orphaned] VNC 后端已清理: backend_id={}", removed);
                    }
                    None => {
                        debug!("⚠️ [orphaned] VNC 后端未找到: identifier={}", info.id);
                    }
                }
            }
        }

        info!(
            "✅ [orphaned] 容器清理成功: {}, 原因={}",
            info.container_name,
            info.reason.as_str()
        );

        Ok(())
    }

    fn infer_service_type_from_pattern(pattern: &str) -> Option<ServiceType> {
        if pattern.contains("rcoder-agent") {
            Some(ServiceType::RCoder)
        } else if pattern.contains("computer-agent-runner") {
            Some(ServiceType::ComputerAgentRunner)
        } else {
            None
        }
    }

    fn extract_id_from_container_name(container_name: &str) -> Option<String> {
        for service_type in [ServiceType::RCoder, ServiceType::ComputerAgentRunner] {
            let prefix = format!("{}-", service_type.container_prefix());
            if let Some(id) = container_name.strip_prefix(&prefix) {
                return Some(id.to_string());
            }
        }
        None
    }
}

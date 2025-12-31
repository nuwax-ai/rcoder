//! 孤立容器清理器
//!
//! 清理 DuckDB 中没有对应记录的孤立容器

use crate::cleanup_task::config::CleanupConfig;
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
    pub async fn cleanup(&self, max_cleanup: u64) -> Result<u64> {
        info!("🔍 [orphaned] 开始检查孤立容器");

        let total_timeout = Duration::from_secs(120);
        let cleaned_count = timeout(total_timeout, self.cleanup_inner(max_cleanup)).await??;

        info!("✅ [orphaned] 孤立容器清理完成: {} 个", cleaned_count);
        Ok(cleaned_count)
    }

    async fn cleanup_inner(&self, max_cleanup: u64) -> Result<u64> {
        if self.container_patterns.is_empty() {
            warn!("⚠️ [orphaned] 没有启用的服务，跳过孤立容器清理");
            return Ok(0);
        }

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
                                let is_orphaned = match service_type {
                                    ServiceType::RCoder => {
                                        // RCoder: id 是 project_id，直接查询
                                        !self.state.projects.contains_key(&id)
                                    }
                                    ServiceType::ComputerAgentRunner => {
                                        // ComputerAgentRunner: id 是 user_id，检查是否有任何项目使用该 user_id
                                        self.state.projects.find_projects_by_user_id(&id).is_empty()
                                    }
                                };

                                if is_orphaned {
                                    // 🔧 修复时间戳解析：Docker 返回毫秒时间戳
                                    let created_time = container.created.and_then(|ts| {
                                        DateTime::from_timestamp(
                                            ts / 1000,
                                            (ts % 1000) as u32 * 1_000_000,
                                        )
                                    });

                                    // 🛡️ 保护期检查：刚启动的容器不应该被清理
                                    if let Some(created) = created_time {
                                        let age = Utc::now().signed_duration_since(created);

                                        if age.num_milliseconds() < self.config.container_protection_duration.as_millis() as i64 {
                                            info!(
                                                "🛡️ [orphaned] 容器在保护期内，跳过清理: {}, 创建时长={}秒",
                                                clean_name,
                                                age.num_seconds()
                                            );
                                            continue; // 跳过此容器，不清理
                                        }
                                    }

                                    return Some(OrphanedContainerInfo {
                                        id,
                                        container_name: clean_name.to_string(),
                                        service_type: service_type.clone(),
                                        created_at: created_time,
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

            let containers_to_clean: Vec<_> = orphaned_containers
                .into_iter()
                .take(max_cleanup as usize)
                .collect();

            for info in containers_to_clean {
                if self.cleanup_single(&info).await.is_ok() {
                    total_cleaned += 1;
                }
            }
        }

        Ok(total_cleaned)
    }

    async fn cleanup_single(&self, info: &OrphanedContainerInfo) -> Result<()> {
        info!(
            "🔥 [orphaned] 开始清理孤立容器: {} (id={}, type={:?})",
            info.container_name, info.id, info.service_type
        );

        // 🛡️ 二次保护期检查：在实际销毁前再次确认容器是否在保护期内
        // 这是为了防止从收集孤立容器列表到实际销毁之间的时间差
        if let Some(container_info) = self
            .docker_manager
            .find_container_by_identifier(&info.container_name)
            .await
        {
            // 检查容器创建时间（created_at 是 DateTime<Utc> 类型）
            let created_time = container_info.created_at;
            let age = Utc::now().signed_duration_since(created_time);

            if age.num_milliseconds() < self.config.container_protection_duration.as_millis() as i64
            {
                info!(
                    "🛡️ [orphaned] 二次检查：容器在保护期内，跳过销毁: {}, 创建时长={}秒",
                    info.container_name,
                    age.num_seconds()
                );
                return Ok(());
            }

            docker_manager::container_stop::runtime_cleanup_container(
                &self.docker_manager,
                &container_info.container_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("清理容器失败: {}", e))?;

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

            info!("✅ [orphaned] 容器清理成功: {}", info.container_name);
        } else {
            info!("📭 [orphaned] 容器不存在: {}", info.container_name);
        }

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

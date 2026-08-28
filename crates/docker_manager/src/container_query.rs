//! Container query operations
//!
//! All container query and inspection methods extracted from DockerManager.
//! This module handles:
//! - Cache lookups (get_container_info, list_containers)
//! - Realtime Docker API queries (find_container_realtime)
//! - Container status checks (find_project_container)
//! - Network and connection info (get_container_network_info, get_container_connection_info)
//! - Pattern-based listings (list_containers_with_pattern)

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use shared_types::ContainerBasicInfo;
use tracing::{debug, error, info, warn};

use crate::{
    ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerError, DockerManager,
    DockerResult,
};

impl DockerManager {
    // ========================================================================
    // Cache-based queries
    // ========================================================================

    /// 获取容器信息（从缓存）
    ///
    /// 从内存缓存中获取容器信息，速度快但可能不是最新的。
    /// 如果需要最新状态，请使用 `find_container_realtime`。
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    ///
    /// # Returns
    /// 容器信息（如果缓存中存在）
    pub async fn get_container_info(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.get(project_id).await
    }

    /// 清理容器缓存
    ///
    /// 从 DockerManager 的内存缓存中移除容器信息。
    /// 通常在容器被销毁后调用，以保持缓存与实际状态同步。
    pub async fn remove_container_cache(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.remove(project_id).await
    }

    /// 获取所有容器信息（从缓存）
    pub async fn list_containers(&self) -> Vec<DockerContainerInfo> {
        self.containers.list().await
    }

    // ========================================================================
    // Realtime Docker API queries
    // ========================================================================

    /// 实时查询容器状态（使用缓存 + 超时保护）
    ///
    /// 此方法直接查询 Docker API 获取最新的容器状态（返回的 container_id 保证新鲜）。
    ///
    /// 🔧 优化：使用 Moka 缓存减少 Docker API 调用，使用超时保护防止阻塞
    /// 📝 缓存策略：同时缓存 container_id 和 container_name，支持 404 响应缓存
    ///
    /// # 参数
    /// * `identifier` - 容器名称或容器 ID
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(ContainerQueryResult)`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_container_realtime(
        &self,
        identifier: &str,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        debug!(
            "[REALTIME] Getting container status: identifier={}",
            identifier
        );

        // 1. 尝试从缓存获取（只缓存成功结果，不缓存 404）
        if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
            debug!("[REALTIME] cache hit: identifier={}", identifier);
            // Arc::clone 只是增加引用计数，开销很小
            return Ok(Some((*cached).clone()));
        }

        // 2. 缓存未命中，调用 Docker API（带超时）
        let timeout = Duration::from_secs(self.config.api_timeout_quick_seconds);
        let result = match self.inspect_with_timeout(identifier, timeout).await {
            Ok(details) => {
                // 解析结果
                let container_id = details.id.unwrap_or_default();
                let container_name = details
                    .name
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| identifier.to_string());

                let (status, is_running) = if let Some(state) = details.state {
                    if let Some(state_status) = state.status {
                        let is_running =
                            state_status == bollard::models::ContainerStateStatusEnum::RUNNING;
                        let status = ContainerStatus::from(state_status.to_string());
                        (status, is_running)
                    } else {
                        (ContainerStatus::Unknown("no status".to_string()), false)
                    }
                } else {
                    (ContainerStatus::Unknown("no state".to_string()), false)
                };

                // 🔧 获取容器 IP（用于 gRPC 连接）
                let container_ip = match self.get_container_network_info(&container_id).await {
                    Ok(network_ips) => {
                        let network_name = self.get_main_network_name().await;
                        network_ips
                            .get(&network_name)
                            .cloned()
                            .or_else(|| network_ips.values().next().cloned())
                            .unwrap_or_default()
                    }
                    Err(e) => {
                        warn!(
                            "[REALTIME] Failed to get container AP, will retry later: container_id={}, error={}",
                            container_id, e
                        );
                        String::new()
                    }
                };

                // 🔧 解析容器创建时间（从 Docker API 的 RFC3339 字符串）
                let created_at = details
                    .created
                    .as_deref()
                    .and_then(|s| Self::parse_rfc3339_timestamp(s, "realtime_created").ok())
                    .unwrap_or_else(|| {
                        warn!(
                            "[REALTIME] Failed to parse container created time, using current time: container_id={}",
                            container_id
                        );
                        Utc::now()
                    });

                // 🔧 使用 Arc 包装，减少 clone 开销
                let query_result = ContainerQueryResult::new(
                    container_id.clone(),
                    container_name.clone(),
                    status,
                    is_running,
                    container_ip,
                    created_at,
                );
                let result_arc = Arc::new(query_result);

                // 同时用 container_id 和 container_name 作为缓存 key
                // Arc::clone 只是增加引用计数，开销很小
                self.api_cache
                    .insert_status(container_id.clone(), Some(result_arc.clone()))
                    .await;
                self.api_cache
                    .insert_status(container_name.clone(), Some(result_arc.clone()))
                    .await;

                info!(
                    "[REALTIME] Container status query succeeded: id={}, name={}, status={:?}, running={}, ip={}",
                    container_id,
                    container_name,
                    result_arc.status,
                    result_arc.is_running,
                    result_arc.container_ip
                );

                // 返回解引用后的值（因为返回类型不是 Arc）
                Some((*result_arc).clone())
            }
            Err(DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })) => {
                // 🔧 修复：不再缓存 404 响应
                // 原因：容器可能刚被创建，缓存 404 会导致 SSE 连接时序问题
                // 容器状态变化快，404 缓存收益小但风险大
                // 同时清理 network_cache 避免残留旧 IP（容器重建后 IP 可能变化）
                self.api_cache.invalidate(identifier).await;
                debug!(
                    "[REALTIME] Container does not exist (not caching 404): identifier={}",
                    identifier
                );
                None
            }
            Err(DockerError::Timeout(_)) => {
                warn!(
                    "[REALTIME] Query timeout, trying to get from cache: identifier={}",
                    identifier
                );
                // 超时时，尝试返回缓存中的旧值（如果有的话）
                if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
                    return Ok(Some((*cached).clone()));
                }
                return Err(DockerError::Timeout(format!(
                    "Container status query timeout with no available cache: identifier={}",
                    identifier
                )));
            }
            Err(e) => {
                error!(
                    "[REALTIME] Query container status failed: identifier={}, error={}",
                    identifier, e
                );
                return Err(e);
            }
        };

        Ok(result)
    }

    /// 解析 RFC3339 时间戳字符串
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 RFC3339 时间戳解析
    ///
    /// # 参数
    /// * `timestamp_str` - RFC3339 格式的时间戳字符串
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    pub(crate) fn parse_rfc3339_timestamp(
        timestamp_str: &str,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::parse_from_rfc3339(timestamp_str)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                format!(
                    "Failed to parse RFC3339 timestamp for {}: '{}', error: {}",
                    context, timestamp_str, e
                )
            })
    }

    /// 解析 Unix 秒时间戳
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 Unix 秒时间戳解析
    /// 用于 `list_containers` API 返回的 created 字段
    ///
    /// # 参数
    /// * `timestamp_secs` - Unix 秒时间戳
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    ///
    /// # 注意
    /// Docker 的 list_containers API 返回的是 Unix **秒**时间戳，不是毫秒
    #[cfg(test)]
    pub(crate) fn parse_unix_timestamp(
        timestamp_secs: i64,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::from_timestamp(timestamp_secs, 0).ok_or_else(|| {
            format!(
                "Failed to parse Unix timestamp for {}: {} (out of range)",
                context, timestamp_secs
            )
        })
    }

    /// 查找项目容器
    ///
    /// 根据 project_id 和 service_type 查找容器：
    /// - 容器命名规则：`{prefix}-{project_id}`
    /// - RCoder 模式前缀：`rcoder-agent`
    /// - ComputerAgentRunner 模式前缀：`computer-agent-runner`
    ///
    /// # 参数
    /// * `project_id` - 项目 ID
    /// * `service_type` - 服务类型
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(ContainerQueryResult)`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_project_container(
        &self,
        project_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        // 1. 查 DashMap 缓存 (如果存在且运行中，通过容器名查询 IP)
        if let Some(info) = self.containers.get(project_id).await {
            // 🎯 验证 service_type 是否匹配
            // 避免 WebAgentRunner 容器被错误地用于 ComputerAgentRunner 请求
            if let Some(ref container_service_type) = info.service_type {
                if container_service_type != service_type {
                    debug!(
                        "[FIND_CONTAINER] Service type mismatch: expected={:?}, found={:?}, container={}, skipping",
                        service_type, container_service_type, info.container_name
                    );
                    // 继续查找，不返回这个容器
                } else {
                    // service_type 匹配，继续检查容器状态
                    let is_running = matches!(info.status, ContainerStatus::Running);
                    if !is_running {
                        return Ok(Some(ContainerQueryResult::new(
                            info.container_id.clone(),
                            info.container_name.clone(),
                            info.status.clone(),
                            false,
                            String::new(),
                            info.created_at,
                        )));
                    }

                    // 容器运行中，通过容器名查询获取 IP（Moka API 缓存优先，miss 时才调 Docker API）
                    // 如果无法获取 IP（容器已被销毁但 DashMap 缓存未清理），标记为非运行状态
                    let (container_ip, effective_status) = match self
                        .find_container_realtime(&info.container_name)
                        .await
                    {
                        Ok(Some(realtime_info)) if !realtime_info.container_ip.is_empty() => {
                            (realtime_info.container_ip, info.status.clone())
                        }
                        Ok(Some(realtime_info)) => {
                            warn!(
                                "[FIND_CONTAINER] Container in DashMap marked Running but has empty IP, treating as stopped: container_name={}, container_id={}",
                                info.container_name, info.container_id
                            );
                            (realtime_info.container_ip, ContainerStatus::Stopped)
                        }
                        _ => {
                            warn!(
                                "[FIND_CONTAINER] Container in DashMap marked Running but not found via Docker API, treating as stopped: container_name={}, container_id={}",
                                info.container_name, info.container_id
                            );
                            (String::new(), ContainerStatus::Stopped)
                        }
                    };

                    let is_running = matches!(effective_status, ContainerStatus::Running);
                    return Ok(Some(ContainerQueryResult::new(
                        info.container_id.clone(),
                        info.container_name.clone(),
                        effective_status,
                        is_running,
                        container_ip,
                        info.created_at,
                    )));
                }
            }
        }

        // 2. 实时查询 Docker API (构造名称)
        // UserApp/UserAppBuilder 短路多镜像配置查询：这两类从不配置 service image
        // （get_service_config 必 Err 且每次深克隆 MultiImageConfig + warn 日志），
        // 前缀直接取 ServiceType 常量——runtime 终端代理每请求走此路径。
        let prefix = match service_type {
            shared_types::ServiceType::UserApp | shared_types::ServiceType::UserAppBuilder => {
                service_type.container_prefix().to_string()
            }
            _ => match self.get_service_config(service_type).await {
                Ok(config) => config.container_prefix().to_string(),
                Err(e) => {
                    warn!(
                        "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                        service_type, e
                    );
                    service_type.container_prefix().to_string()
                }
            },
        };
        // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
        let expected_container_name =
            crate::utils::DockerUtils::generate_container_name(&prefix, project_id)
                .map_err(DockerError::ConfigurationError)?;

        // 直接返回 find_container_realtime 的结果
        self.find_container_realtime(&expected_container_name).await
    }

    /// 获取 Agent 容器的高级信息
    ///
    /// 封装了容器查找、IP解析、URL构建和信息转换逻辑
    /// 替代 rcoder 层的手动拼装逻辑
    pub async fn get_agent_info(
        &self,
        project_id: &str,
    ) -> DockerResult<Option<ContainerBasicInfo>> {
        // 1. 查找容器信息（内存映射）
        let container_info = match self.get_container_info(project_id).await {
            Some(info) => info,
            None => return Ok(None),
        };

        // 2. 获取容器 IP (优先使用主网络)
        // 注意：如果容器已被外部删除（如手动 docker rm），此处会出错
        let network_name = self.get_main_network_name().await;
        let network_ips = match self
            .get_container_network_info(&container_info.container_id)
            .await
        {
            Ok(ips) => ips,
            Err(e) => {
                // 检查是否是容器不存在的错误（404 状态码）
                // 容器已被外部删除，清理内存映射并返回 None
                // 这样上层调用者可以重新创建容器
                if matches!(
                    &e,
                    DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    })
                ) {
                    warn!(
                        "[GET_AGENT_INFO] Container was externally deleted (status 404), cleaning up memory mapping: project_id={}, container_id={}",
                        project_id, container_info.container_id
                    );
                    self.containers.remove(project_id).await;
                    return Ok(None);
                }
                // 其他错误正常传播
                return Err(e);
            }
        };

        // 如果网络信息为空，说明容器可能已被删除或未正确连接到网络
        // 清理内存映射并返回 None，让上层调用者重新创建容器
        if network_ips.is_empty() {
            warn!(
                "[GET_AGENT_INFO] Container has no network info (may have been deleted), cleaning up memory mapping: project_id={}, container_id={}",
                project_id, container_info.container_id
            );
            self.containers.remove(project_id).await;
            return Ok(None);
        }

        let container_ip = network_ips
            .get(&network_name)
            .cloned()
            .or_else(|| network_ips.values().next().cloned())
            .ok_or_else(|| {
                DockerError::ConnectionError("Container not connected to any network".to_string())
            })?;

        // 3. 构建服务 URL (Agent 内部默认监听 HTTP_DEFAULT_PORT=8086)
        let server_url = format!(
            "http://{}:{}",
            container_ip,
            shared_types::HTTP_DEFAULT_PORT
        );

        // 4. 转换并返回
        Ok(Some(ContainerBasicInfo {
            container_id: container_info.container_id,
            container_name: container_info.container_name,
            container_ip,
            internal_port: container_info.internal_port,
            external_port: container_info.assigned_port,
            project_id: container_info.project_id,
            status: container_info.status.to_string(),
            created_at: container_info.created_at,
            service_url: server_url,
        }))
    }

    /// 获取容器的连接信息 (IP)
    ///
    /// 用于清理任务获取资源回收所需的信息
    pub async fn get_container_connection_info(
        &self,
        container_info: &DockerContainerInfo,
    ) -> DockerResult<Option<String>> {
        // 1. 获取 IP
        let ip_addr = match self
            .get_container_network_info(&container_info.container_id)
            .await
        {
            Ok(network_ips) => network_ips
                .get(&container_info.network_name)
                .cloned()
                .or_else(|| network_ips.values().next().cloned()),
            Err(e) => {
                warn!("get container ip failed: {}", e);
                None
            }
        };

        Ok(ip_addr)
    }

    // ========================================================================
    // ComputerAgentRunner 专用接口
    // ========================================================================
    //
    // ComputerAgentRunner 模式与 RCoder 模式不同：
    // - 容器命名：computer-agent-runner-{user_id}（而非 project_id）
    // - 一个 user_id 对应一个容器
    // - 容器内可以运行多个 project_id 的 Agent 实例
    //
    // 以下接口专门用于 ComputerAgentRunner 模式，参数名更清晰，
    // 避免与 RCoder 模式的 project_id 参数混淆。

    /// 查找用户容器（ComputerAgentRunner 模式专用）
    ///
    /// 根据 user_id 和 service_type 查找容器：
    /// - 容器命名规则：`{prefix}-{user_id}`
    /// - ComputerAgentRunner 模式前缀：`computer-agent-runner`
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    /// * `service_type` - 服务类型（应该是 ComputerAgentRunner）
    ///
    /// # 返回
    /// * `Ok(Some(ContainerQueryResult))` - 容器存在
    /// * `Ok(None)` - 容器不存在
    /// * `Err(...)` - 查询出错
    pub async fn find_user_container(
        &self,
        user_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        // 1. 查 Map (如果存在且运行中，直接返回)
        if let Some(info) = self.containers.get(user_id).await {
            return Ok(Some(ContainerQueryResult::new(
                info.container_id.clone(),
                info.container_name.clone(),
                info.status.clone(),
                matches!(info.status, ContainerStatus::Running),
                String::new(), // 缓存命中时 IP 可能已过期，依赖后续实时查询更新
                info.created_at,
            )));
        }

        // 2. 实时查询 Docker API (构造名称)
        // UserApp/UserAppBuilder 短路多镜像配置查询：这两类从不配置 service image
        // （get_service_config 必 Err 且每次深克隆 MultiImageConfig + warn 日志），
        // 前缀直接取 ServiceType 常量——runtime 终端代理每请求走此路径。
        let prefix = match service_type {
            shared_types::ServiceType::UserApp | shared_types::ServiceType::UserAppBuilder => {
                service_type.container_prefix().to_string()
            }
            _ => match self.get_service_config(service_type).await {
                Ok(config) => config.container_prefix().to_string(),
                Err(e) => {
                    warn!(
                        "[FIND_CONTAINER] Failed to get service config, using default prefix: service_type={:?}, error={}",
                        service_type, e
                    );
                    service_type.container_prefix().to_string()
                }
            },
        };
        // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
        let expected_container_name =
            crate::utils::DockerUtils::generate_container_name(&prefix, user_id)
                .map_err(DockerError::ConfigurationError)?;

        // 直接返回 find_container_realtime 的结果
        self.find_container_realtime(&expected_container_name).await
    }

    /// 列出匹配指定模式的容器
    ///
    /// 使用 Docker API 列出所有容器（包括停止的），并根据名称模式过滤。
    /// 自动排除当前容器自身（通过 HOSTNAME 环境变量识别）。
    ///
    /// # Arguments
    /// * `pattern` - 容器名称匹配模式
    ///
    /// # Returns
    /// 匹配的容器列表
    pub async fn list_containers_with_pattern(
        &self,
        pattern: &str,
    ) -> DockerResult<Vec<bollard::models::ContainerSummary>> {
        info!("Listing containers: pattern={}", pattern);

        // 🎯 获取当前容器的 ID（用于排除自己）
        let current_container_id = std::env::var("HOSTNAME").ok();

        // 使用 Docker API 列出所有容器（包括停止的）
        let options = Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        });

        let containers = self.docker.list_containers(options).await.map_err(|e| {
            DockerError::ConnectionError(format!("failed to get container list: {}", e))
        })?;

        // 创建过滤器
        let filter = crate::ContainerFilter::name_pattern(pattern);

        let total_count = containers.len();
        // 过滤容器，排除当前容器自己（直接用 into_iter 消费，避免 clone）
        let matched_containers: Vec<bollard::models::ContainerSummary> = containers
            .into_iter()
            .filter(|container| {
                // 排除当前容器自己
                if let Some(ref current_id) = current_container_id
                    && let Some(ref container_id) = container.id
                {
                    // HOSTNAME 是容器 ID 的前 12 位
                    if container_id.starts_with(current_id) {
                        info!("skip removing container: {}", container_id);
                        return false;
                    }
                }
                filter.matches(container)
            })
            .collect();

        info!(
            "Container lookup completed: total={}, matched={} (self excluded), pattern={}",
            total_count,
            matched_containers.len(),
            pattern
        );

        Ok(matched_containers)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use bollard::Docker;
    use bollard::query_parameters::InspectContainerOptions;
    use chrono::{DateTime, Utc};

    /// 测试通过容器名称获取创建时间
    ///
    /// 使用真实容器 `rcoder-rcoder-1` 验证时间戳解析
    #[tokio::test]
    #[ignore] // 需要本地环境有 Docker 和容器，默认忽略
    #[allow(deprecated)] // 测试代码使用 deprecated API 是可接受的
    async fn test_get_container_creation_time_by_name_real() {
        // 直接使用 Bollard 创建 Docker 客户端
        let docker = Docker::connect_with_local_defaults().expect("Failed to connect to Docker");

        // 测试容器名称
        let container_name = "rcoder-rcoder-1";

        println!("\n🔍 checking container: {}", container_name);
        println!("─────────────────────────────────────────");

        // 直接调用 Docker API 获取容器信息
        match docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                println!("✅ succeeded getcontainer");

                // 获取创建时间字符串
                if let Some(ref created_str) = details.created {
                    println!(" Docker API created: {}", created_str);

                    // 解析时间戳
                    match DateTime::parse_from_rfc3339(created_str) {
                        Ok(created_time) => {
                            let created_time_utc = created_time.with_timezone(&Utc);
                            println!(" created UTC: {}", created_time_utc);

                            // 计算容器年龄
                            let age = Utc::now().signed_duration_since(created_time_utc);
                            println!(" container age (seconds): {}", age.num_seconds());
                            println!(" container age (minutes): {}", age.num_minutes());
                            println!(" container age (hours): {}", age.num_hours());
                            println!(" container age (days): {}", age.num_days());

                            // 验证时间是否合理
                            assert!(created_time_utc < Utc::now(), "创建时间应该在过去");
                            assert!(age.num_days() < 365, "创建时间不应该超过 1 年");

                            println!("\n✅ timestamp test passed!");
                        }
                        Err(e) => {
                            panic!("❌ RFC3339 时间戳解析失败: {}", e);
                        }
                    }
                } else {
                    panic!("❌ 容器没有 created 字段");
                }

                // 使用 Docker CLI 对比验证
                println!("\n🔍 checking Docker CLI:");
                println!("─────────────────────────────────────────");

                use std::process::Command;
                let output = Command::new("docker")
                    .args(["inspect", container_name, "--format", "{{.Created}}"])
                    .output()
                    .expect("Failed to run docker inspect");

                let docker_cli_time = String::from_utf8_lossy(&output.stdout);
                println!(" Docker CLI created: {}", docker_cli_time.trim());

                // 解析 Docker CLI 返回的时间
                if let Ok(docker_time) = DateTime::parse_from_rfc3339(docker_cli_time.trim()) {
                    let docker_time_utc = docker_time.with_timezone(&Utc);
                    println!("   Docker CLI UTC: {}", docker_time_utc);

                    // 从 Docker API 获取的时间
                    if let Some(ref created_str) = details.created
                        && let Ok(api_time) = DateTime::parse_from_rfc3339(created_str)
                    {
                        let api_time_utc = api_time.with_timezone(&Utc);
                        println!(" API created UTC: {}", api_time_utc);

                        // 时间差应该为 0（应该完全一致）
                        let diff = (docker_time_utc.timestamp() - api_time_utc.timestamp()).abs();
                        println!(" time diff: {} seconds", diff);

                        assert_eq!(diff, 0, "API 和 CLI 返回的时间应该完全一致");
                        println!("\n✅ Docker CLI check passed!");
                    }
                }
            }
            Err(e) => {
                panic!("❌ 获取容器信息失败: {}", e);
            }
        }
    }

    /// 测试 Unix 时间戳解析（验证 bug 修复）
    #[tokio::test]
    #[ignore]
    #[allow(deprecated)] // 测试代码使用 deprecated API 是可接受的
    async fn test_unix_timestamp_parsing() {
        use chrono::TimeZone;

        println!("\n🔍 testing Unix timestamp ( old bug )");
        println!("─────────────────────────────────────────");

        // 容器实际创建时间: 2026-01-19T07:35:53Z
        let expected_time = Utc.with_ymd_and_hms(2026, 1, 19, 7, 35, 53).unwrap();
        let unix_timestamp = expected_time.timestamp(); // 1768808153 秒

        println!(" expected time: {}", expected_time);
        println!(" unix timestamp: {}", unix_timestamp);

        // 使用我们的解析函数
        match DockerManager::parse_unix_timestamp(unix_timestamp, "test") {
            Ok(parsed_time) => {
                println!(" parsed time: {}", parsed_time);

                let diff = (parsed_time.timestamp() - expected_time.timestamp()).abs();
                println!(" time diff: {} seconds", diff);

                assert_eq!(diff, 0, "时间戳解析应该完全准确");
                println!("\n✅ Unix timestamp test passed!");
            }
            Err(e) => {
                panic!("❌ 解析失败: {}", e);
            }
        }

        // 验证旧代码的错误
        println!("\n🔍 verifying bug:");
        let wrong_seconds = unix_timestamp / 1000; // 旧代码的错误处理
        let wrong_time = Utc.timestamp_opt(wrong_seconds, 0).single().unwrap();
        println!(" wrong time: {} (error!)", wrong_time);
        println!(
            "   与正确时间相差: {} 天",
            (expected_time.timestamp() - wrong_time.timestamp()) / 86400
        );
    }

    /// 测试时间戳解析的完整流程
    ///
    /// 主动创建一个测试容器，同时使用 list_containers 和 inspect_container API
    /// 验证 parse_unix_timestamp 和 parse_rfc3339_timestamp 的正确性
    #[tokio::test]
    #[ignore] // 需要本地 Docker 环境
    async fn test_timestamp_parsing_with_real_container() {
        use bollard::models::ContainerCreateBody;
        use bollard::query_parameters::{
            CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
            RemoveContainerOptionsBuilder,
        };
        use futures_util::TryStreamExt;

        // 连接 Docker
        let docker = Docker::connect_with_local_defaults().expect("Failed to connect to Docker");

        // 测试容器名称（使用时间戳避免冲突）
        let container_name = format!("test-timestamp-{}", chrono::Utc::now().timestamp());

        println!("\n🔍 testing timestamp parsing");
        println!("─────────────────────────────────────────");
        println!(" testing container: {}", container_name);

        // 拉取 alpine 镜像（如果不存在）
        println!("\n📥 pulling image: alpine:latest");
        let create_image_options = CreateImageOptionsBuilder::default()
            .from_image("alpine:latest")
            .build();

        drop(
            docker
                .create_image(Some(create_image_options), None, None)
                .try_collect::<Vec<_>>()
                .await,
        );

        // 1. 创建测试容器（使用 alpine 镜像）
        let config = ContainerCreateBody {
            image: Some("alpine:latest".to_string()),
            cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
            host_config: Some(bollard::models::HostConfig {
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptionsBuilder::default()
            .name(&container_name)
            .build();

        let create_result = docker
            .create_container(Some(create_options), config)
            .await
            .expect("Failed to create test container");

        println!("✅ container already created: {}", create_result.id);

        // 2. 启动容器
        docker
            .start_container(
                &container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .expect("Failed to start test container");

        println!("✅ container already started");

        // 等待容器完全启动
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 3. 使用 list_containers API 获取 Unix 时间戳
        println!("\n📋 testing list_containers API (Unix timestamp):");
        println!("─────────────────────────────────────────");

        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec![container_name.clone()]);

        let list_options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();

        let containers = docker
            .list_containers(Some(list_options))
            .await
            .expect("Failed to list containers");

        assert_eq!(containers.len(), 1, "应该只找到一个测试容器");
        let container = &containers[0];

        let unix_timestamp = container.created.expect("容器应该有 created 字段");
        println!(" unix timestamp: {} seconds", unix_timestamp);

        // 使用 parse_unix_timestamp 解析
        let parsed_unix_time = DockerManager::parse_unix_timestamp(
            unix_timestamp,
            &format!("container {}", container_name),
        )
        .expect("parse_unix_timestamp 应该成功");

        println!(" parsed (UTC): {}", parsed_unix_time);

        // 4. 使用 inspect_container API 获取 RFC3339 时间戳
        println!("\n📋 testing inspect_container API (RFC3339 timestamp):");
        println!("─────────────────────────────────────────");

        let details = docker
            .inspect_container(&container_name, None::<InspectContainerOptions>)
            .await
            .expect("Failed to inspect container");

        let rfc3339_str = details.created.expect("容器应该有 created 字段");
        println!(" RFC3339 timestamp: {}", rfc3339_str);

        // 使用 parse_rfc3339_timestamp 解析
        let parsed_rfc3339_time = DockerManager::parse_rfc3339_timestamp(
            &rfc3339_str,
            &format!("container {}", container_name),
        )
        .expect("parse_rfc3339_timestamp 应该成功");

        println!(" parsed (UTC): {}", parsed_rfc3339_time);

        // 5. 验证两个解析结果的一致性
        println!("\n🔍 comparing API results:");
        println!("─────────────────────────────────────────");

        let time_diff = (parsed_unix_time.timestamp() - parsed_rfc3339_time.timestamp()).abs();
        println!(" list_containers parsed: {}", parsed_unix_time);
        println!(" inspect_container parsed: {}", parsed_rfc3339_time);
        println!(" time diff: {} seconds", time_diff);

        // 两个 API 应该返回相同的时间（允许 1 秒误差，因为精度不同）
        assert!(
            time_diff <= 1,
            "两个 API 的时间差应该在 1 秒以内，实际差异: {} 秒",
            time_diff
        );

        // 6. 验证时间合理性
        println!("\n🔍 verifying timestamps:");
        println!("─────────────────────────────────────────");

        let now = Utc::now();
        let age = now.signed_duration_since(parsed_unix_time);

        println!(" current time: {}", now);
        println!(" container age (seconds): {}", age.num_seconds());

        assert!(age.num_seconds() >= 0, "容器创建时间应该在过去");
        assert!(age.num_seconds() < 60, "容器应该是刚创建的（< 60 秒）");

        println!("\n✅ timestamp test passed!");

        // 7. 清理测试容器
        println!("\n🧹 cleaning up test container...");

        let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();

        docker
            .remove_container(&container_name, Some(remove_options))
            .await
            .expect("Failed to cleanup test container");

        println!("✅ container already cleaned up: {}", container_name);
    }
}

use super::{
    CleanupOptions, CleanupResult, ContainerRemovalFailure, ContainerStatus, DockerContainerConfig,
    DockerContainerInfo, DockerError, DockerManagerConfig, DockerResult,
};
use crate::container_state_actor::{ContainerStateActor, ContainerStateHandle};
use anyhow::Result;
use bollard::query_parameters::{
    InspectContainerOptions, RemoveContainerOptions, RestartContainerOptions, StopContainerOptions,
};
use bollard::{API_DEFAULT_VERSION, Docker, models::ContainerSummary};
use container_runtime_api::RemovedContainerInfo;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::api_cache::DockerApiCache;

/// Docker 容器管理器
pub struct DockerManager {
    /// Docker 客户端
    pub(crate) docker: Docker,
    /// 管理器配置
    pub(crate) config: DockerManagerConfig,
    /// 容器状态句柄（Actor 模式，无锁并发安全）
    pub(crate) containers: ContainerStateHandle,
    /// 主网络名称（动态检测或使用默认值）
    pub(crate) main_network_name: std::sync::Arc<tokio::sync::RwLock<String>>,
    /// Docker API 缓存
    pub(crate) api_cache: Arc<DockerApiCache>,
}

impl DockerManager {
    /// 创建新的 Docker 管理器
    pub async fn new(config: DockerManagerConfig) -> DockerResult<Self> {
        let docker = if let Some(host) = &config.docker_host {
            Docker::connect_with_http(host, 120, API_DEFAULT_VERSION)?
        } else {
            Docker::connect_with_local_defaults()?
        };

        // 测试连接
        docker.ping().await.map_err(|e| {
            DockerError::ConnectionError(format!("unable to connect to Docker daemon: {}", e))
        })?;

        info!("Docker manager initialized");

        // 🔍 动态检测主网络名称（必须成功）
        let main_network_name =
            match Self::detect_main_network_name_static(&docker, &config.network_base_name).await {
                Ok(network_name) => {
                    info!("detected network: {}", network_name);
                    network_name
                }
                Err(e) => {
                    error!("unable to detect network: {}", e);
                    return Err(e);
                }
            };

        // 🆕 创建容器状态 Actor 并启动
        let (actor, containers) = ContainerStateActor::new();
        tokio::spawn(actor.run());
        info!("ContainerStateActor already started");

        // 🗄️ 初始化 Docker API 缓存（使用配置的 TTL 和容量）
        let api_cache = Arc::new(DockerApiCache::new(
            config.cache_status_ttl_seconds,
            config.cache_network_ttl_seconds,
            config.cache_max_capacity,
        ));

        let manager = Self {
            docker,
            config,
            containers,
            main_network_name: std::sync::Arc::new(tokio::sync::RwLock::new(main_network_name)),
            api_cache,
        };

        // 确保 RCoder 网络存在
        manager.ensure_rcoder_network().await?;

        Ok(manager)
    }

    /// 使用默认配置创建 Docker 管理器
    pub async fn with_default_config() -> DockerResult<Self> {
        Self::new(DockerManagerConfig::default()).await
    }

    /// 带超时的 inspect_container 调用
    ///
    /// 封装 Docker API 调用，添加超时保护，防止请求阻塞
    pub(crate) async fn inspect_with_timeout(
        &self,
        identifier: &str,
        timeout: Duration,
    ) -> DockerResult<bollard::models::ContainerInspectResponse> {
        tokio::time::timeout(
            timeout,
            self.docker
                .inspect_container(identifier, None::<InspectContainerOptions>),
        )
        .await
        .map_err(|_| {
            DockerError::Timeout(format!(
                "Docker API call timeout ({}s): identifier={}",
                timeout.as_secs(),
                identifier
            ))
        })?
        .map_err(DockerError::BollardError)
    }

    /// 创建并启动容器
    pub async fn create_container(
        &self,
        config: DockerContainerConfig,
    ) -> DockerResult<DockerContainerInfo> {
        crate::container_creator::ContainerCreator::new(self)
            .create(config)
            .await
    }

    /// 通过容器ID停止容器
    pub(crate) async fn stop_container_by_id(&self, container_id: &str) -> DockerResult<()> {
        self.stop_container_by_id_with_timeout(container_id, 30)
            .await
    }

    /// 通过容器ID停止容器（带超时参数）
    pub async fn stop_container_by_id_with_timeout(
        &self,
        container_id: &str,
        timeout_seconds: u64,
    ) -> DockerResult<()> {
        info!(
            "Quick destroy container: {} (timeout: {}s)",
            container_id, timeout_seconds
        );

        // 先检查容器是否真实存在，避免删除不存在的容器导致错误
        // 使用 inspect API 检查容器状态
        let timeout = Duration::from_secs(timeout_seconds);
        let container_exists = match tokio::time::timeout(
            timeout,
            self.docker
                .inspect_container(container_id, None::<InspectContainerOptions>),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                // 根据 bollard 错误类型判断：DockerResponseServerError 包含 status_code
                // 404 表示容器不存在，这是正常的（可能已被外部清理）
                // 其他错误则记录警告但继续尝试删除
                match &e {
                    bollard::errors::Error::DockerResponseServerError { status_code, .. }
                        if *status_code == 404 =>
                    {
                        info!(
                            "Container {} does not exist in Docker (status 404, already cleaned up), skipping destroy",
                            container_id
                        );
                        return Ok(());
                    }
                    _ => {
                        warn!(
                            "Failed to inspect container {} before destroy: {}, will try remove anyway",
                            container_id, e
                        );
                        true // 继续尝试删除
                    }
                }
            }
            Err(_) => {
                // 超时，假设容器可能已不存在，尝试删除
                warn!(
                    "Timeout inspecting container {} ({}s), will try remove anyway",
                    container_id, timeout_seconds
                );
                true
            }
        };

        if !container_exists {
            info!("Container {} does not exist, destroy skipped", container_id);
            return Ok(());
        }

        // 🚀 直接使用 force remove，无需先 stop
        // force: true 会自动停止运行中的容器
        // 这样可以避免 "removal already in progress" 的竞态问题
        let remove_options = Some(RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        });

        self.docker
            .remove_container(container_id, remove_options)
            .await
            .map_err(|e| {
                warn!("Failed to remove container {}: {}", container_id, e);
                DockerError::BollardError(e)
            })?;

        info!("containerdestroysucceeded: {}", container_id);

        // 从映射中移除所有匹配 container_id 的条目
        // 注意：同一容器可能存储在多个 key 下（如 project_id 和 pod_id），
        // 必须遍历所有条目，不能 break，否则会遗留孤儿缓存
        for info in self.containers.list().await {
            if info.container_id == container_id {
                self.containers.remove(&info.project_id).await;
                self.api_cache.invalidate(container_id).await;
                self.api_cache
                    .invalidate(info.container_name.as_str())
                    .await;
            }
        }

        Ok(())
    }

    /// 停止并删除容器
    pub async fn stop_container(&self, project_id: &str) -> DockerResult<()> {
        info!("stoppedcontainer, projectID: {}", project_id);

        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
        } else {
            warn!("Project {} has no container", project_id);
            return Ok(());
        };

        // 调用通过ID停止的方法（已包含缓存失效和映射移除）
        self.stop_container_by_id(&container_info.container_id)
            .await?;

        // 从映射中移除
        self.containers.remove(project_id).await;

        Ok(())
    }

    /// 启动 Agent 容器（全流程封装）
    ///
    /// 替代 rcoder 层的复杂编排逻辑
    pub async fn start_agent_container(
        &self,
        params: container_runtime_api::ContainerCreateParams,
    ) -> DockerResult<ContainerBasicInfo> {
        crate::agent_container_starter::AgentContainerStarter::new(self)
            .start(params)
            .await
    }

    /// 检查并更新容器状态
    pub async fn update_container_status(
        &self,
        project_id: &str,
    ) -> DockerResult<Option<ContainerStatus>> {
        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
        } else {
            return Ok(None);
        };

        // 查询容器状态
        match self
            .docker
            .inspect_container(
                &container_info.container_id,
                None::<InspectContainerOptions>,
            )
            .await
        {
            Ok(details) => {
                if let Some(state) = details.state {
                    let status = state
                        .status
                        .map(|s| ContainerStatus::from(s.to_string()))
                        .unwrap_or(ContainerStatus::Unknown("unknown".to_string()));

                    // 更新状态
                    let mut info = container_info;
                    info.status = status.clone();
                    info.health_status = state.health.and_then(|h| h.status.map(|s| s.to_string()));

                    self.containers.insert(project_id.to_string(), info).await;

                    Ok(Some(status))
                } else {
                    Ok(Some(ContainerStatus::Unknown("no state".to_string())))
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在（HTTP 404），从映射中移除
                self.containers.remove(project_id).await;
                Ok(None)
            }
            Err(e) => Err(DockerError::BollardError(e)),
        }
    }

    /// 同步所有缓存容器的状态
    ///
    /// 遍历缓存中的所有容器，调用 Docker API 检查其真实状态。
    /// 如果容器已被外部删除（如手动 `docker stop`），则从缓存中移除。
    /// 🆕 对运行中的容器执行服务健康检查（HTTP + gRPC）
    ///
    /// # Returns
    /// 返回元组 (已检查数量, 已移除容器信息列表)
    pub async fn sync_all_container_states(
        &self,
    ) -> DockerResult<(u32, Vec<RemovedContainerInfo>)> {
        // 获取所有 project_id 的快照
        let project_ids: Vec<String> = self.containers.keys().await;

        if project_ids.is_empty() {
            return Ok((0, Vec::new()));
        }

        let total = project_ids.len() as u32;
        let mut removed = Vec::new();
        let mut health_checked_count = 0u32;

        // 创建健康检查器（复用同一个实例）
        let health_checker = Arc::new(crate::health::ServiceHealthChecker::new());

        // 提前获取主网络名称（避免每个并发任务重复获取 RwLock）
        let main_network_name = self.get_main_network_name().await;

        // 🚀 并发批处理：每批最多 10 个容器
        let batch_size = 10;
        for chunk in project_ids.chunks(batch_size) {
            let futures: Vec<_> = chunk
                .iter()
                .map(|project_id| {
                    let health_checker = health_checker.clone();
                    let main_network_name = main_network_name.clone();
                    async move {
                        let project_id = project_id.clone();
                        let container_info_before_update = self.containers.get(&project_id).await;

                        match self.update_container_status(&project_id).await {
                            Ok(None) => {
                                // 容器不存在，需要从缓存中移除
                                if let Some(info) = container_info_before_update {
                                    // 获取容器 IP（用于清理 gRPC 连接池）
                                    let container_ip = match self
                                        .get_container_network_info(&info.container_id)
                                        .await
                                    {
                                        Ok(ips) => ips.values().next().cloned().unwrap_or_default(),
                                        Err(e) => {
                                            warn!(
                                                "[SYNC] Failed to get container IP for cleanup: container_id={}, error={}",
                                                info.container_id, e
                                            );
                                            String::new()
                                        }
                                    };

                                    Some((project_id.clone(), Some(RemovedContainerInfo {
                                        container_name: info.container_name,
                                        container_ip,
                                        identifier: project_id,
                                        service_type: info.service_type.unwrap_or(ServiceType::WebAgentRunner),
                                    }), false))
                                } else {
                                    Some((project_id, None, false))
                                }
                            }
                            Ok(Some(status)) => {
                                // 🆕 对运行中的容器执行服务健康检查
                                if matches!(status, ContainerStatus::Running) {
                                    let container_info = self.containers.get(&project_id).await;
                                    let Some(container_info) = container_info else {
                                        return Some((project_id, None, false));
                                    };

                                    // 获取容器 IP
                                    if let Ok(network_ips) = self
                                        .get_container_network_info(&container_info.container_id)
                                        .await
                                    {
                                        let container_ip = network_ips
                                            .get(&main_network_name)
                                            .or_else(|| network_ips.values().next());

                                        if let Some(ip) = container_ip {
                                            let previous_failures = container_info
                                                .service_health
                                                .as_ref()
                                                .map(|h| h.consecutive_failures)
                                                .unwrap_or(0);

                                            let health_status =
                                                health_checker.check_service(ip, previous_failures).await;

                                            let mut updated_info = container_info.clone();
                                            updated_info.service_health = Some(health_status.clone());
                                            self.containers
                                                .insert(project_id.clone(), updated_info)
                                                .await;

                                            if health_status.is_fully_healthy() {
                                                debug!(
                                                    "[SYNC] Service healthy: container_id={}, service_type={:?}",
                                                    project_id, container_info.service_type
                                                );
                                            } else {
                                                warn!(
                                                    "[SYNC] Service unhealthy: container_id={}, service_type={:?}, http={}, grpc={}, failures={}",
                                                    project_id,
                                                    container_info.service_type,
                                                    health_status.http_healthy,
                                                    health_status.grpc_healthy,
                                                    health_status.consecutive_failures
                                                );
                                            }

                                            return Some((project_id, None, true));
                                        }
                                    }
                                }
                                Some((project_id, None, false))
                            }
                            Err(e) => {
                                warn!(
                                    "[SYNC] Check container status failed: project_id={}, error={}",
                                    project_id, e
                                );
                                None
                            }
                        }
                    }
                })
                .collect();

            // 并发执行当前批次
            let results = futures_util::future::join_all(futures).await;
            for (project_id, removed_info, health_checked) in results.into_iter().flatten() {
                if let Some(info) = removed_info {
                    info!(
                        "[SYNC] Container removed from cache (does not exist in Docker): project_id={}",
                        project_id
                    );
                    removed.push(info);
                }
                if health_checked {
                    health_checked_count += 1;
                }
            }
        }

        if !removed.is_empty() || health_checked_count > 0 {
            info!(
                "[SYNC] Container status sync completed: checked={}, removed={}, health_checked={}",
                total,
                removed.len(),
                health_checked_count
            );
        }

        // 清理被移除容器的 API 缓存（避免残留数据导致后续查询返回旧 IP）
        if !removed.is_empty() {
            let identifiers: Vec<String> = removed.iter().map(|r| r.identifier.clone()).collect();
            self.api_cache.invalidate_all(&identifiers).await;
            debug!(
                "[SYNC] Invalidated API cache for {} removed containers",
                identifiers.len()
            );
        }

        Ok((total, removed))
    }

    /// 清理所有容器
    pub async fn cleanup_all_containers(&self) -> DockerResult<()> {
        info!("Starting cleanup of all containers");

        let project_ids: Vec<String> = self.containers.keys().await;

        for project_id in project_ids {
            if let Err(e) = self.stop_container(&project_id).await {
                error!("cleanup project {} container failed: {}", project_id, e);
            }
        }

        info!("Container cleanup completed");
        Ok(())
    }

    /// 重启容器
    pub async fn restart_container(&self, project_id: &str) -> DockerResult<()> {
        info!("Creating container, projectID: {}", project_id);

        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
        } else {
            return Err(DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("project {} has no corresponding container", project_id),
            )));
        };

        self.docker
            .restart_container(
                &container_info.container_id,
                None::<RestartContainerOptions>,
            )
            .await
            .map_err(|e| {
                DockerError::ContainerStartError(format!("failed to restart container: {}", e))
            })?;

        // 🔧 使缓存失效（容器状态已变更）
        self.api_cache
            .invalidate(container_info.container_id.as_str())
            .await;
        self.api_cache
            .invalidate(container_info.container_name.as_str())
            .await;

        info!("Container created: {}", container_info.container_name);
        Ok(())
    }

    /// 获取 Docker 客户端实例
    pub fn get_docker_client(&self) -> &Docker {
        &self.docker
    }

    /// 获取容器网络信息（使用缓存 + 超时保护）
    ///
    /// 🔧 优化：使用 Moka 缓存减少 Docker API 调用，使用超时保护防止阻塞
    /// 📝 缓存策略：支持 None 值缓存（容器不存在或网络信息为空时）
    ///
    /// # 返回
    /// - `Ok(HashMap)`: 网络名称到 IP 地址的映射（可能为空）
    /// - `Err(ConnectionError)`: 容器不存在或无法获取网络信息
    pub async fn get_container_network_info(
        &self,
        container_id: &str,
    ) -> DockerResult<HashMap<String, String>> {
        // 1. 尝试从缓存获取
        if let Some(Some(cached)) = self.api_cache.get_network(container_id).await {
            debug!(
                "[NETWORK] getting network info: container_id={}",
                container_id
            );
            // Arc::clone 只是增加引用计数，解引用后 clone HashMap
            return Ok((*cached).clone());
        }

        // 1.5 检查是否缓存了 None（空网络信息）
        if let Some(None) = self.api_cache.get_network(container_id).await {
            debug!(
                "[NETWORK] Cache hit (empty network): container_id={}",
                container_id
            );
            return Ok(HashMap::new());
        }

        // 2. 缓存未命中，调用 Docker API（带超时）
        let timeout = Duration::from_secs(self.config.api_timeout_quick_seconds);
        let inspect = match self.inspect_with_timeout(container_id, timeout).await {
            Ok(i) => i,
            Err(DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })) => {
                // 容器不存在 - 缓存空 HashMap
                debug!(
                    "[NETWORK] Container does not exist, caching empty network: container_id={}",
                    container_id
                );
                self.api_cache
                    .insert_network(container_id.to_string(), None)
                    .await;
                return Ok(HashMap::new());
            }
            Err(DockerError::Timeout(_)) => {
                warn!(
                    "[NETWORK] Query timeout, trying to get from cache: container_id={}",
                    container_id
                );
                // 超时时，尝试返回缓存中的旧值（如果有的话）
                if let Some(Some(cached)) = self.api_cache.get_network(container_id).await {
                    return Ok((*cached).clone());
                }
                return Err(DockerError::Timeout(format!(
                    "Container network info query timeout and no available cache: container_id={}",
                    container_id
                )));
            }
            Err(e) => return Err(e),
        };

        // 3. 解析网络信息
        let mut network_ips = HashMap::new();

        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks
        {
            for (network_name, network_info) in networks {
                if let Some(ip_address) = network_info.ip_address
                    && !ip_address.is_empty()
                {
                    network_ips.insert(network_name, ip_address);
                }
            }
        }

        // 4. 写入缓存（如果为空也缓存，避免重复查询）
        let result_to_cache = if network_ips.is_empty() {
            None
        } else {
            // 🔧 使用 Arc 包装，减少 clone 开销
            Some(Arc::new(network_ips.clone()))
        };
        self.api_cache
            .insert_network(container_id.to_string(), result_to_cache)
            .await;

        Ok(network_ips)
    }

    /// 检查容器健康状态
    pub(crate) async fn check_container_health(&self, container_id: &str) -> DockerResult<()> {
        use bollard::query_parameters::InspectContainerOptions;

        // 检查容器详细信息
        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!("failed to check container status: {}", e))
            })?;

        // 检查容器状态
        if let Some(state) = inspect.state {
            let status = state.status;
            let exit_code = state.exit_code.unwrap_or(-1);

            match status {
                Some(bollard::models::ContainerStateStatusEnum::RUNNING) => {
                    info!("Container {} is running", container_id);
                    Ok(())
                }
                Some(bollard::models::ContainerStateStatusEnum::EXITED) => {
                    let error_msg = state.error.as_deref().unwrap_or("unknown error");
                    error!(
                        "Container {} exited (exit code: {}): {}",
                        container_id, exit_code, error_msg
                    );
                    Err(DockerError::ContainerStartError(format!(
                        "Container exited immediately after startup: {} (exit code: {}), error: {}",
                        container_id, exit_code, error_msg
                    )))
                }
                Some(bollard::models::ContainerStateStatusEnum::CREATED) => {
                    warn!("container {} already created but not started", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "Container created but not started: {}",
                        container_id
                    )))
                }
                Some(status) => {
                    let status_str = format!("{:?}", status);
                    error!(
                        "Container {} has unexpected status: {}",
                        container_id, status_str
                    );
                    Err(DockerError::ContainerStartError(format!(
                        "Container in unknown state: {} - {}",
                        container_id, status_str
                    )))
                }
                None => {
                    error!("Container {} status is empty", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "Container status is empty: {}",
                        container_id
                    )))
                }
            }
        } else {
            error!("Unable to get container {} status", container_id);
            Err(DockerError::ContainerStartError(format!(
                "unable to get container status info: {}",
                container_id
            )))
        }
    }

    /// 批量停止并删除指定的容器
    ///
    /// # Arguments
    /// * `container_ids` - 要删除的容器ID列表
    /// * `options` - 清理选项
    ///
    /// # Returns
    /// 返回清理操作结果统计
    pub async fn stop_and_remove_containers_by_ids(
        &self,
        container_ids: Vec<String>,
        options: CleanupOptions,
    ) -> DockerResult<CleanupResult> {
        info!("Starting cleanup container: count={}", container_ids.len());

        let start_time = Instant::now();
        let mut result = CleanupResult {
            total_found: container_ids.len(),
            ..Default::default()
        };

        for container_id in &container_ids {
            match self
                .stop_and_remove_single_container(container_id, &options)
                .await
            {
                Ok(_) => {
                    result.successfully_removed += 1;
                    result.removed_container_ids.push(container_id.clone());
                    info!("Container cleanup succeeded: {}", container_id);
                }
                Err(e) => {
                    result.failed_removals += 1;
                    result
                        .failed_removals_details
                        .push(ContainerRemovalFailure {
                            container_id: container_id.clone(),
                            container_name: container_id.clone(), // 我们可能不知道名称，使用ID
                            error_message: e.to_string(),
                        });
                    error!("Container cleanup failed: {} - {}", container_id, e);
                }
            }
        }

        result.duration_ms = start_time.elapsed().as_millis().min(u64::MAX as u128) as u64;

        info!(
            "Batch container cleanup completed: total={}, success={}, failed={}, duration={}ms",
            result.total_found,
            result.successfully_removed,
            result.failed_removals,
            result.duration_ms
        );

        Ok(result)
    }

    /// 停止并删除单个容器
    async fn stop_and_remove_single_container(
        &self,
        container_id: &str,
        options: &CleanupOptions,
    ) -> DockerResult<()> {
        info!("Cleaning up container: {}", container_id);

        // 第一步：获取容器信息
        let container_info = self.inspect_container_for_cleanup(container_id).await?;

        // 第二步：检查容器状态并决定是否需要停止
        match container_info
            .state
            .as_ref()
            .and_then(|s| s.status.as_ref())
        {
            Some(status) if status.to_string() == "running" => {
                if !options.force_remove_running {
                    info!("Container {} is running, skip (force=false)", container_id);
                    return Ok(());
                }

                if options.wait_for_graceful_stop {
                    info!("Gracefully stopped container: {}", container_id);
                    if let Err(e) = self
                        .graceful_stop_container(container_id, options.stop_timeout_seconds)
                        .await
                    {
                        warn!(
                            "gracefulstoppedfailed, forcestopped: {} - {}",
                            container_id, e
                        );
                        // 强制停止
                        self.force_stop_container(container_id).await?;
                    }
                } else {
                    // 直接强制停止
                    self.force_stop_container(container_id).await?;
                }
            }
            Some(_) => {
                info!("Container {} is not running", container_id);
            }
            None => {
                warn!("Unable to get container {} status", container_id);
            }
        }

        // 第三步：删除容器
        self.remove_single_container(container_id, options.remove_associated_volumes)
            .await?;

        info!("containercleanupcompleted: {}", container_id);
        Ok(())
    }

    /// 获取容器信息用于清理
    async fn inspect_container_for_cleanup(
        &self,
        container_id: &str,
    ) -> Result<bollard::models::ContainerInspectResponse, DockerError> {
        let options = Some(InspectContainerOptions { size: false });

        self.docker
            .inspect_container(container_id, options)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!("failed to get container info: {}", e))
            })
    }

    /// 优雅停止容器
    async fn graceful_stop_container(
        &self,
        container_id: &str,
        timeout_seconds: u64,
    ) -> DockerResult<()> {
        let stop_options = Some(StopContainerOptions {
            t: Some(timeout_seconds as i32),
            signal: None::<String>,
        });

        self.docker
            .stop_container(container_id, stop_options)
            .await
            .map_err(|e| {
                DockerError::ContainerStopError(format!(
                    "failed to gracefully stop container: {}",
                    e
                ))
            })
    }

    /// 强制停止容器
    async fn force_stop_container(&self, container_id: &str) -> DockerResult<()> {
        let stop_options = Some(StopContainerOptions {
            t: None::<i32>,
            signal: None::<String>,
        });

        self.docker
            .stop_container(container_id, stop_options)
            .await
            .map_err(|e| {
                DockerError::ContainerStopError(format!("failed to force stop container: {}", e))
            })
    }

    /// 删除单个容器
    async fn remove_single_container(
        &self,
        container_id: &str,
        remove_volumes: bool,
    ) -> DockerResult<()> {
        let remove_options = Some(RemoveContainerOptions {
            force: true,
            v: remove_volumes,
            ..Default::default()
        });

        self.docker
            .remove_container(container_id, remove_options)
            .await
            .map_err(|e| {
                DockerError::ContainerRemoveError(format!("failed to delete container: {}", e))
            })
    }

    /// 使用模式匹配清理容器（主要接口）
    ///
    /// # Arguments
    /// * `pattern` - 容器名称模式（如 "rcoder-agent-*"）
    /// * `options` - 清理选项
    ///
    /// # Returns
    /// 返回清理结果统计
    pub async fn cleanup_containers_with_pattern(
        &self,
        pattern: &str,
        options: CleanupOptions,
    ) -> DockerResult<CleanupResult> {
        info!("Starting cleanup container: pattern={:?}", pattern);

        // 第一步：查找匹配的容器
        let matched_containers = self.list_containers_with_pattern(pattern).await?;

        // 第二步：提取容器ID
        let container_ids: Vec<String> = matched_containers
            .iter()
            .filter_map(|container| container.id.as_ref())
            .cloned()
            .collect();

        info!(
            "Found {} matching containers: pattern={}",
            container_ids.len(),
            pattern
        );

        // 第三步：批量清理
        let result = self
            .stop_and_remove_containers_by_ids(container_ids, options)
            .await;

        // 第四步：从内部映射中移除已清理的容器
        self.cleanup_internal_mappings(&matched_containers).await;

        result
    }

    /// 从内部映射中清理已删除的容器
    async fn cleanup_internal_mappings(&self, removed_containers: &[ContainerSummary]) {
        for container in removed_containers {
            if let Some(container_id) = &container.id {
                // 从内存映射中查找并移除
                // 从内存映射中查找并安全移除
                for info in self.containers.list().await {
                    if info.container_id == *container_id {
                        // 使用安全移除，只有 container_id 匹配时才移除 (防止误删重启后的新容器)
                        if self
                            .containers
                            .remove_if_container_id(&info.project_id, container_id)
                            .await
                            .is_some()
                        {
                            info!(
                                "Removed from internal mapping: project_id={}, container_id={}",
                                info.project_id, container_id
                            );
                        }
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for DockerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerManager")
            .field("containers", &"ContainerStateHandle (async)")
            .field("config", &self.config)
            .finish()
    }
}

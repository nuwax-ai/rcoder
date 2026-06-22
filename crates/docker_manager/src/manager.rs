use super::{
    CleanupOptions, CleanupResult, ContainerQueryResultArc, ContainerRemovalFailure,
    ContainerStatus, DockerContainerConfig, DockerContainerInfo, DockerError, DockerManagerConfig,
    DockerResult,
};
use crate::container_state_actor::{ContainerStateActor, ContainerStateHandle};
use anyhow::Result;
use bollard::query_parameters::{
    CreateImageOptions, InspectContainerOptions, RemoveContainerOptions, RestartContainerOptions,
    StopContainerOptions,
};
use bollard::{API_DEFAULT_VERSION, Docker, models::ContainerSummary};
use container_runtime_api::RemovedContainerInfo;
use moka::future::Cache;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Docker API 缓存
///
/// 使用 Moka 缓存库实现高性能缓存，减少 Docker API 调用次数
/// 使用结构体包装，提高代码可读性和减少 clone 开销
pub struct DockerApiCache {
    /// 容器状态缓存 (identifier -> Option<ContainerQueryResultArc>)
    /// 支持 None 值缓存，用于缓存 404 响应
    status_cache: Cache<String, Option<ContainerQueryResultArc>>,

    /// 网络信息缓存 (container_id -> Option<Arc<HashMap<network_name, ip_address>>>)
    /// 支持 None 值缓存
    network_cache: Cache<String, Option<Arc<HashMap<String, String>>>>,
}

impl DockerApiCache {
    /// 创建新的缓存实例
    ///
    /// # 参数
    /// * `status_ttl` - 状态缓存 TTL（秒）
    /// * `network_ttl` - 网络缓存 TTL（秒）
    /// * `max_capacity` - 缓存最大容量
    pub fn new(status_ttl: u64, network_ttl: u64, max_capacity: u64) -> Self {
        info!(
            "Initializing Docker API cache: status_ttl={}s, network_ttl={}s, max_capacity={}",
            status_ttl, network_ttl, max_capacity
        );

        Self {
            status_cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(status_ttl))
                .build(),
            network_cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(network_ttl))
                .build(),
        }
    }

    /// 使用默认配置创建缓存实例
    #[allow(dead_code)]
    pub fn with_defaults() -> Self {
        Self::new(10, 15, 10000)
    }

    /// 获取状态缓存
    pub async fn get_status(&self, identifier: &str) -> Option<Option<ContainerQueryResultArc>> {
        self.status_cache.get(identifier).await
    }

    /// 写入状态缓存（支持 None 值）
    pub async fn insert_status(&self, identifier: String, value: Option<ContainerQueryResultArc>) {
        self.status_cache.insert(identifier, value).await;
    }

    /// 获取网络缓存
    pub async fn get_network(
        &self,
        container_id: &str,
    ) -> Option<Option<Arc<HashMap<String, String>>>> {
        self.network_cache.get(container_id).await
    }

    /// 写入网络缓存（支持 None 值）
    pub async fn insert_network(
        &self,
        container_id: String,
        value: Option<Arc<HashMap<String, String>>>,
    ) {
        self.network_cache.insert(container_id, value).await;
    }

    /// 使缓存失效
    pub async fn invalidate(&self, identifier: &str) {
        self.status_cache.invalidate(identifier).await;
        self.network_cache.invalidate(identifier).await;
    }

    /// 使所有相关缓存失效（用于容器生命周期变化后）
    pub async fn invalidate_all(&self, identifiers: &[String]) {
        for id in identifiers {
            self.status_cache.invalidate(id.as_str()).await;
            self.network_cache.invalidate(id.as_str()).await;
        }
    }
}

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
                    info!("detecting network: {}", network_name);
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

    /// 确保镜像存在，如果不存在则拉取
    pub(crate) async fn ensure_image_exists(&self, image: &str) -> DockerResult<()> {
        debug!("Checking if image exists: {}", image);

        // 检查镜像是否存在
        match self.docker.inspect_image(image).await {
            Ok(_) => {
                debug!("Image {} already exists", image);
                Ok(())
            }
            Err(_) => {
                info!("Image {} not found, pulling...", image);

                let pull_options = CreateImageOptions {
                    from_image: Some(image.to_string()),
                    ..Default::default()
                };

                let mut pull_stream = self.docker.create_image(Some(pull_options), None, None);

                while let Some(result) = pull_stream.next().await {
                    match result {
                        Ok(progress) => {
                            if let Some(status) = progress.status {
                                debug!("container status: {}", status);
                            }
                        }
                        Err(e) => {
                            return Err(DockerError::ImagePullError(format!(
                                "Failed to pull image: {}",
                                e
                            )));
                        }
                    }
                }

                info!("Image {} pull completed", image);
                Ok(())
            }
        }
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

    /// 确保 RCoder 网络存在
    async fn ensure_rcoder_network(&self) -> DockerResult<()> {
        let main_network = self.get_main_network_name().await;
        info!("Checking RCoder network status: {}...", main_network);

        // 检查网络是否已存在
        match self.inspect_network(&main_network).await {
            Ok(_) => {
                info!("RCoder network already exists: {}", main_network);
                Ok(())
            }
            Err(_) => {
                warn!("RCoder network not found: {}", main_network);
                warn!("Network mode: bridge");
                warn!("Please check Docker Compose config");
                // 不创建网络，因为主网络应该由 Docker Compose 创建
                Ok(())
            }
        }
    }

    /// 检查网络是否存在
    async fn inspect_network(&self, network_name: &str) -> DockerResult<()> {
        use bollard::query_parameters::ListNetworksOptions;

        // 使用 list_networks 不带参数，然后手动过滤
        let networks = self
            .docker
            .list_networks(None::<ListNetworksOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("failed to list networks: {}", e)))?;

        if networks
            .iter()
            .any(|n| n.name.as_ref() == Some(&network_name.to_string()))
        {
            Ok(())
        } else {
            Err(DockerError::ConnectionError(
                "Network does not exist".to_string(),
            ))
        }
    }

    /// 获取 Docker 客户端实例
    pub fn get_docker_client(&self) -> &Docker {
        &self.docker
    }

    /// 获取配置的默认镜像
    pub fn get_default_image(&self) -> String {
        self.config.default_image.clone()
    }

    /// 根据服务类型选择镜像
    pub async fn select_image(
        &self,
        service_type: &shared_types::ServiceType,
        project_overrides: Option<&shared_types::ProjectImageOverrides>,
    ) -> DockerResult<String> {
        // 使用多镜像配置选择镜像
        use crate::image_selector::ImageSelector;
        let selector = ImageSelector::new(self.config.multi_image_config.clone());

        debug!(" ImageSelector: {:?}", service_type);
        selector.select_image(service_type, project_overrides).await
    }

    /// 获取服务配置
    pub async fn get_service_config(
        &self,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<shared_types::ServiceImageConfig> {
        use crate::image_selector::ImageSelector;
        let selector = ImageSelector::new(self.config.multi_image_config.clone());

        debug!("Getting config: {:?}", service_type);
        selector.get_service_config(service_type).await
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
                "📭 [NETWORK] Cache hit (empty network): container_id={}",
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
                    "📭 [NETWORK] Container does not exist, caching empty network: container_id={}",
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
        info!(
            "🔥 Starting cleanup container: count={}",
            container_ids.len()
        );

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
                    info!("✅ Container cleanup succeeded: {}", container_id);
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
                    error!("❌ Container cleanup failed: {} - {}", container_id, e);
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
                    info!(
                        "⚠️ Container {} is running, skip (force=false)",
                        container_id
                    );
                    return Ok(());
                }

                if options.wait_for_graceful_stop {
                    info!("🛑 Gracefully stopped container: {}", container_id);
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
        info!("🧹 Starting cleanup container: pattern={:?}", pattern);

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

    /// 获取主网络名称（异步，返回动态检测的值）
    pub(crate) async fn get_main_network_name(&self) -> String {
        self.main_network_name.read().await.clone()
    }

    /// 🔍 动态检测当前主容器所在的网络名称（静态方法，用于初始化）
    ///
    /// 通过检查当前容器（运行 DockerManager 的容器）所连接的网络来确定主网络名称
    /// 这样可以适应不同的 Docker Compose project name
    async fn detect_main_network_name_static(
        docker: &Docker,
        network_base_name: &str,
    ) -> DockerResult<String> {
        use bollard::query_parameters::InspectContainerOptions;

        // 🎯 优化：直接通过 HOSTNAME 环境变量 inspect 当前容器，无需列出所有容器
        let hostname = std::env::var("HOSTNAME").map_err(|_| {
            DockerError::ConnectionError(
                "unable to get HOSTNAME environment variable. Please ensure code is running in Docker container.".to_string(),
            )
        })?;

        debug!("Detecting container hostname: {}", hostname);

        // 直接 inspect 当前容器（hostname 通常是容器 ID 的前12位，但 Docker API 支持前缀匹配）
        let inspect = docker
            .inspect_container(&hostname, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!(
                    "unable to get current container info (hostname: {}): {}",
                    hostname, e
                ))
            })?;

        // 获取网络配置
        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks
        {
            // 查找包含指定网络基础名称的网络
            for network_name in networks.keys() {
                if network_name.contains(network_base_name) {
                    info!("detected network: {}", network_name);
                    return Ok(network_name.clone());
                }
            }

            // 如果没找到包含指定基础名称的，回退到第一个可用网络（Docker Compose 默认网络）
            if let Some(fallback_name) = networks.keys().next() {
                warn!(
                    "未找到包含 '{}' 的网络，回退使用当前容器的默认网络: {} (可用网络: {:?})",
                    network_base_name,
                    fallback_name,
                    networks.keys().collect::<Vec<_>>()
                );
                return Ok(fallback_name.clone());
            }

            return Err(DockerError::ConnectionError(
                "当前容器没有任何网络配置".to_string(),
            ));
        }

        Err(DockerError::ConnectionError(format!(
            "Current container (hostname: {}) has no network configuration information",
            hostname
        )))
    }

    /// 🔍 动态检测当前主容器所在的网络名称
    ///
    /// 通过检查当前容器（运行 DockerManager 的容器）所连接的网络来确定主网络名称
    /// 这样可以适应不同的 Docker Compose project name
    pub async fn detect_main_network_name(&self) -> DockerResult<String> {
        Self::detect_main_network_name_static(&self.docker, &self.config.network_base_name).await
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

/// 为了支持 futures Stream，需要导入 StreamExt trait
use futures_util::stream::StreamExt;

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::Docker;
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
            .inspect_container(
                container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
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

        let _ = docker
            .create_image(Some(create_image_options), None, None)
            .try_collect::<Vec<_>>()
            .await;

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
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 3. 使用 list_containers API 获取 Unix 时间戳
        println!("\n📋 testing list_containers API (Unix timestamp):");
        println!("─────────────────────────────────────────");

        let mut filters = std::collections::HashMap::new();
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

use super::{
    ContainerStatus, DockerContainerConfig, DockerContainerInfo, DockerError, DockerManagerConfig,
    DockerResult,
};
use crate::container_state_actor::{ContainerStateActor, ContainerStateHandle};
use bollard::query_parameters::{
    InspectContainerOptions, RemoveContainerOptions, RestartContainerOptions,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use shared_types::ContainerBasicInfo;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

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
    pub(crate) main_network_name: Arc<tokio::sync::RwLock<String>>,
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
            main_network_name: Arc::new(tokio::sync::RwLock::new(main_network_name)),
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

        match tokio::time::timeout(
            timeout,
            self.docker.remove_container(container_id, remove_options),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!("Failed to remove container {}: {}", container_id, e);
                return Err(DockerError::BollardError(e));
            }
            Err(_) => {
                // 与上方 inspect 一致的超时保护：daemon 挂起/OOM/网络阻塞时避免永久阻塞
                warn!(
                    "Timed out removing container {} after {}s",
                    container_id, timeout_seconds
                );
                return Err(DockerError::Timeout(format!(
                    "remove container {container_id} ({timeout_seconds}s)"
                )));
            }
        }

        info!("container destroy succeeded: {}", container_id);

        // 单次 actor 往返移除所有匹配 container_id 的条目（替代 list()+逐个 remove 的 O(n²)）。
        // 同一容器可能存于多个 key（project_id/pod_id），一次清空避免孤儿缓存。
        let removed = self
            .containers
            .remove_all_by_container_id(container_id)
            .await;
        if !removed.is_empty() {
            self.api_cache.invalidate(container_id).await;
            for info in &removed {
                self.api_cache
                    .invalidate(info.container_name.as_str())
                    .await;
            }
        }

        Ok(())
    }

    /// 停止并删除容器
    pub async fn stop_container(&self, project_id: &str) -> DockerResult<()> {
        info!("stopped container, project_id: {}", project_id);

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
}

// 大方法 extension 拆分（子模块可访问本模块私有字段）
mod network;
mod sync;

impl std::fmt::Debug for DockerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerManager")
            .field("containers", &"ContainerStateHandle (async)")
            .field("config", &self.config)
            .finish()
    }
}

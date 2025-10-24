use super::{
    CleanupOptions, CleanupResult, ContainerFilter, ContainerRemovalFailure, ContainerStatus,
    DockerContainerConfig, DockerContainerInfo, DockerError, DockerManagerConfig, DockerResult,
    MountPoint, RCODER_NETWORK_NAME,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    LogsOptions, RemoveContainerOptions, RestartContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::{
    API_DEFAULT_VERSION, Docker,
    models::{
        ContainerCreateBody, ContainerSummary, HostConfig, Mount, Network, NetworkingConfig,
        PortBinding,
    },
};
use chrono::Utc;
use dashmap::DashMap;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Docker 容器管理器
pub struct DockerManager {
    /// Docker 客户端
    docker: Docker,
    /// 管理器配置
    config: DockerManagerConfig,
    /// 容器映射: project_id -> container_info
    containers: DashMap<String, DockerContainerInfo>,
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
            DockerError::ConnectionError(format!("无法连接到 Docker 守护进程: {}", e))
        })?;

        info!("Docker 管理器初始化成功");

        let manager = Self {
            docker,
            config,
            containers: DashMap::new(),
        };

        // 确保 RCoder 网络存在
        manager.ensure_rcoder_network().await?;

        Ok(manager)
    }

    /// 使用默认配置创建 Docker 管理器
    pub async fn with_default_config() -> DockerResult<Self> {
        Self::new(DockerManagerConfig::default()).await
    }

    /// 创建并启动容器
    pub async fn create_container(
        &self,
        config: DockerContainerConfig,
    ) -> DockerResult<DockerContainerInfo> {
        info!("开始创建容器，项目ID: {}", config.project_id);

        // 生成容器名称：使用工具函数统一维护
        let container_name = super::utils::DockerUtils::generate_container_name(
            &config.name_prefix,
            &config.project_id,
        );

        // 检查是否已存在该项目的容器
        if let Some(existing) = self.containers.get(&config.project_id) {
            warn!(
                "项目 {} 已存在容器 {}，将先停止并删除",
                config.project_id, existing.container_name
            );
            if let Err(e) = self.stop_container(&config.project_id).await {
                error!("停止现有容器失败: {}", e);
            }
        }

        // 拉取镜像（如果本地不存在）
        self.ensure_image_exists(&config.image).await?;

        // 创建挂载点
        let mut mounts = vec![Mount {
            target: Some(config.container_path.clone()),
            source: Some(config.host_path.clone()),
            typ: Some(bollard::models::MountTypeEnum::BIND),
            read_only: Some(false),
            ..Default::default()
        }];

        // 添加额外的挂载点
        for extra_mount in &config.extra_mounts {
            mounts.push(Mount {
                target: Some(extra_mount.container_path.clone()),
                source: Some(extra_mount.host_path.clone()),
                typ: Some(bollard::models::MountTypeEnum::BIND),
                read_only: Some(extra_mount.read_only),
                ..Default::default()
            });
        }

        // 构建环境变量
        let env_vars: Vec<String> = config
            .env_vars
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // 构建端口映射
        let mut port_bindings_map = HashMap::new();
        for (container_port, host_port) in &config.port_bindings {
            port_bindings_map.insert(
                container_port.clone(),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(host_port.clone()),
                }]),
            );
        }

        // 创建主机配置 - 不再使用 network_mode，而是通过 NetworkingConfig 连接到网络
        let mut host_config = HostConfig {
            mounts: Some(mounts),
            port_bindings: Some(port_bindings_map),
            auto_remove: Some(config.auto_remove),
            ..Default::default()
        };

        // 应用资源限制
        if let Some(ref limits) = config.resource_limits {
            host_config.memory = limits.memory_limit;
            host_config.memory_swap = limits.swap_limit;
            // CPU 限制需要通过 nano_cpus 设置 (1 CPU = 1e9 nano CPUs)
            if let Some(cpu_limit) = limits.cpu_limit {
                host_config.nano_cpus = Some((cpu_limit * 1e9) as i64);
            }
        }

        // 创建容器配置
        let mut container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            working_dir: Some(config.work_dir.clone()),
            env: Some(env_vars),
            host_config: Some(host_config),
            tty: Some(true),
            open_stdin: Some(true),
            ..Default::default()
        };

        // 设置启动命令
        if let Some(command) = config.command {
            container_config.cmd = Some(command);
        }

        // 设置入口点
        if let Some(entrypoint) = config.entrypoint {
            container_config.entrypoint = Some(entrypoint);
        }

        // 创建容器选项
        let create_options = CreateContainerOptions {
            name: Some(container_name.clone()),
            platform: self.config.default_platform.clone(), // 使用配置中的平台
        };

        // 创建容器
        let create_result = self
            .docker
            .create_container(Some(create_options), container_config)
            .await
            .map_err(|e| DockerError::ContainerCreationError(format!("创建容器失败: {}", e)))?;

        let container_id = create_result.id.clone();

        // 启动容器
        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .map_err(|e| DockerError::ContainerStartError(format!("启动容器失败: {}", e)))?;

        // 连接到 RCoder 网络（如果不是 host 网络模式）
        if config.network_mode != "host" {
            self.connect_container_to_network(&container_id, RCODER_NETWORK_NAME)
                .await?;
        }

        // 等待容器启动完成
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // 检查容器状态，确保容器正在运行
        self.check_container_health(&container_id).await?;

        // 再次等待确保网络配置完成
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // 创建容器信息
        let container_info = DockerContainerInfo {
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            project_id: config.project_id.clone(),
            image: config.image.clone(),
            status: ContainerStatus::Running,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            host_path: config.host_path.clone(),
            container_path: config.container_path.clone(),
            port_bindings: config.port_bindings.clone(),
            assigned_port: 3000, // TODO: 使用动态分配的端口
            health_status: None,
            internal_port: 8080,                    // 默认内部端口
            session_id: Uuid::new_v4().to_string(), // 生成默认会话ID
        };

        // 保存到容器映射
        self.containers
            .insert(config.project_id.clone(), container_info.clone());

        info!(
            "容器创建并启动成功: {} (ID: {})",
            container_name, container_id
        );

        Ok(container_info)
    }

    /// 通过容器ID停止容器
    pub async fn stop_container_by_id(&self, container_id: &str) -> DockerResult<()> {
        info!("通过容器ID停止容器: {}", container_id);

        // 停止容器
        if let Err(e) = self
            .docker
            .stop_container(container_id, None::<StopContainerOptions>)
            .await
        {
            if !e.to_string().contains("No such container") {
                warn!("停止容器 {} 失败: {}", container_id, e);
            }
        }

        // 删除容器
        if let Err(e) = self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    link: false,
                }),
            )
            .await
        {
            if !e.to_string().contains("No such container") {
                warn!("删除容器 {} 失败: {}", container_id, e);
            }
        }

        // 从映射中移除（如果存在）
        self.containers
            .retain(|_, info| info.container_id != container_id);

        Ok(())
    }

    /// 停止并删除容器
    pub async fn stop_container(&self, project_id: &str) -> DockerResult<()> {
        info!("停止容器，项目ID: {}", project_id);

        let container_info = if let Some(info) = self.containers.get(project_id) {
            info.clone()
        } else {
            warn!("项目 {} 没有找到对应的容器", project_id);
            return Ok(());
        };

        // 调用通过ID停止的方法
        self.stop_container_by_id(&container_info.container_id)
            .await?;

        // 从映射中移除
        self.containers.remove(project_id);

        Ok(())
    }

    /// 获取容器信息
    pub fn get_container_info(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.get(project_id).map(|info| info.clone())
    }

    /// 通过多种方式查找容器：project_id 或容器名称
    pub async fn find_container_by_identifier(
        &self,
        identifier: &str,
    ) -> Option<DockerContainerInfo> {
        // 1. 首先尝试通过 project_id 查找
        if let Some(info) = self.containers.get(identifier) {
            return Some(info.clone());
        }

        // 2. 如果没找到，尝试通过容器名称查找
        for entry in self.containers.iter() {
            let info = entry.value();
            if info.container_name == identifier {
                return Some(info.clone());
            }
        }

        // 3. 如果还没找到，尝试通过 Docker API 直接查找容器（适用于容器存在但映射缺失的情况）
        use bollard::container::ListContainersOptions;
        let options = Some(ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        });

        if let Ok(containers) = self.docker.list_containers(options).await {
            for container in containers {
                if let Some(names) = container.names {
                    for name in names {
                        // Docker 容器名称通常以 '/' 开头，需要去掉
                        let clean_name = name.trim_start_matches('/');
                        if clean_name == identifier {
                            let container_id = container.id.clone().unwrap_or_default();
                            info!(
                                "通过 Docker API 找到容器: {} (ID: {})",
                                identifier, container_id
                            );
                            // 创建一个临时的容器信息，用于销毁
                            return Some(DockerContainerInfo {
                                container_id,
                                container_name: clean_name.to_string(),
                                project_id: "unknown".to_string(), // 我们无法直接知道 project_id
                                image: container.image.unwrap_or_default(),
                                status: ContainerStatus::Unknown(
                                    "found_via_docker_api".to_string(),
                                ),
                                created_at: Utc::now(),
                                started_at: None,
                                host_path: String::new(),
                                container_path: String::new(),
                                port_bindings: std::collections::HashMap::new(),
                                assigned_port: 0,
                                health_status: None,
                                internal_port: 0,
                                session_id: String::new(),
                            });
                        }
                    }
                }
            }
        }

        None
    }

    /// 获取所有容器信息
    pub fn list_containers(&self) -> Vec<DockerContainerInfo> {
        self.containers
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 检查并更新容器状态
    pub async fn update_container_status(
        &self,
        project_id: &str,
    ) -> DockerResult<Option<ContainerStatus>> {
        let container_info = if let Some(info) = self.containers.get(project_id) {
            info.clone()
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

                    self.containers.insert(project_id.to_string(), info);

                    Ok(Some(status))
                } else {
                    Ok(Some(ContainerStatus::Unknown("no state".to_string())))
                }
            }
            Err(e) => {
                if e.to_string().contains("No such container") {
                    // 容器不存在，从映射中移除
                    self.containers.remove(project_id);
                    Ok(None)
                } else {
                    Err(DockerError::BollardError(e))
                }
            }
        }
    }

    /// 清理所有容器
    pub async fn cleanup_all_containers(&self) -> DockerResult<()> {
        info!("开始清理所有容器");

        let project_ids: Vec<String> = self
            .containers
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for project_id in project_ids {
            if let Err(e) = self.stop_container(&project_id).await {
                error!("清理项目 {} 的容器失败: {}", project_id, e);
            }
        }

        info!("所有容器清理完成");
        Ok(())
    }

    /// 确保镜像存在，如果不存在则拉取
    async fn ensure_image_exists(&self, image: &str) -> DockerResult<()> {
        debug!("检查镜像是否存在: {}", image);

        // 检查镜像是否存在
        match self.docker.inspect_image(image).await {
            Ok(_) => {
                debug!("镜像 {} 已存在", image);
                Ok(())
            }
            Err(_) => {
                info!("镜像 {} 不存在，开始拉取", image);

                let pull_options = CreateImageOptions {
                    from_image: Some(image.to_string()),
                    ..Default::default()
                };

                let mut pull_stream = self.docker.create_image(Some(pull_options), None, None);

                while let Some(result) = pull_stream.next().await {
                    match result {
                        Ok(progress) => {
                            if let Some(status) = progress.status {
                                debug!("拉取进度: {}", status);
                            }
                        }
                        Err(e) => {
                            return Err(DockerError::ImagePullError(format!(
                                "拉取镜像失败: {}",
                                e
                            )));
                        }
                    }
                }

                info!("镜像 {} 拉取完成", image);
                Ok(())
            }
        }
    }

    /// 获取容器日志
    pub async fn get_container_logs(&self, project_id: &str, lines: i64) -> DockerResult<String> {
        let container_info = if let Some(info) = self.containers.get(project_id) {
            info.clone()
        } else {
            return Err(DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("项目 {} 没有对应的容器", project_id),
            )));
        };

        let log_options = LogsOptions {
            stdout: true,
            stderr: true,
            tail: lines.to_string(),
            timestamps: true,
            ..Default::default()
        };

        let mut log_stream = self
            .docker
            .logs(&container_info.container_id, Some(log_options));
        let mut logs = String::new();

        while let Some(result) = log_stream.next().await {
            match result {
                Ok(output) => {
                    logs.push_str(&String::from_utf8_lossy(&output.into_bytes()));
                }
                Err(e) => {
                    warn!("获取容器日志失败: {}", e);
                }
            }
        }

        Ok(logs)
    }

    /// 重启容器
    pub async fn restart_container(&self, project_id: &str) -> DockerResult<()> {
        info!("重启容器，项目ID: {}", project_id);

        let container_info = if let Some(info) = self.containers.get(project_id) {
            info.clone()
        } else {
            return Err(DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("项目 {} 没有对应的容器", project_id),
            )));
        };

        self.docker
            .restart_container(
                &container_info.container_id,
                None::<RestartContainerOptions>,
            )
            .await
            .map_err(|e| DockerError::ContainerStartError(format!("重启容器失败: {}", e)))?;

        info!("容器重启成功: {}", container_info.container_name);
        Ok(())
    }

    /// 确保 RCoder 网络存在
    async fn ensure_rcoder_network(&self) -> DockerResult<()> {
        info!("检查 RCoder 网络状态...");

        // 检查网络是否已存在
        match self.inspect_network(RCODER_NETWORK_NAME).await {
            Ok(_) => {
                info!("RCoder 网络已存在: {}", RCODER_NETWORK_NAME);
                Ok(())
            }
            Err(_) => {
                info!("RCoder 网络不存在，正在创建...");
                self.create_rcoder_network().await
            }
        }
    }

    /// 创建 RCoder 网络
    async fn create_rcoder_network(&self) -> DockerResult<()> {
        use bollard::network::{CreateNetworkOptions, PruneNetworksOptions};

        let created_time = Utc::now().to_rfc3339();
        let network_config = CreateNetworkOptions {
            name: RCODER_NETWORK_NAME,
            driver: "bridge",
            check_duplicate: true,
            internal: false,
            attachable: true,
            ingress: false,
            ipam: Default::default(),
            enable_ipv6: false,
            options: HashMap::from([
                ("com.docker.network.bridge.name", "rcoder-br0"),
                ("com.docker.network.bridge.enable_icc", "true"),
                ("com.docker.network.bridge.enable_ip_masquerade", "true"),
            ]),
            labels: HashMap::from([
                ("com.rcoder.network", "true"),
                ("com.rcoder.network.created", &created_time),
            ]),
        };

        match self.docker.create_network(network_config).await {
            Ok(_) => {
                info!("✅ RCoder 网络创建成功: {}", RCODER_NETWORK_NAME);
                Ok(())
            }
            Err(e) => {
                error!("❌ RCoder 网络创建失败: {}", e);
                Err(DockerError::ContainerCreationError(format!(
                    "创建网络失败: {}",
                    e
                )))
            }
        }
    }

    /// 检查网络是否存在
    async fn inspect_network(&self, network_name: &str) -> DockerResult<()> {
        use bollard::network::ListNetworksOptions;

        let options = ListNetworksOptions {
            filters: HashMap::from([("name", vec![network_name])]),
        };

        let networks = self
            .docker
            .list_networks(Some(options))
            .await
            .map_err(|e| DockerError::ConnectionError(format!("列出网络失败: {}", e)))?;

        if networks
            .iter()
            .any(|n| n.name.as_ref() == Some(&network_name.to_string()))
        {
            Ok(())
        } else {
            Err(DockerError::ConnectionError("网络不存在".to_string()))
        }
    }

    /// 连接容器到指定网络
    async fn connect_container_to_network(
        &self,
        container_id: &str,
        network_name: &str,
    ) -> DockerResult<()> {
        use bollard::network::ConnectNetworkOptions;

        let connect_config = ConnectNetworkOptions {
            container: container_id.to_string(),
            endpoint_config: Default::default(),
        };

        match self
            .docker
            .connect_network(network_name, connect_config)
            .await
        {
            Ok(_) => {
                info!("✅ 容器 {} 已连接到网络: {}", container_id, network_name);
                Ok(())
            }
            Err(e) => {
                error!("❌ 容器连接网络失败: {}", e);
                Err(DockerError::ContainerCreationError(format!(
                    "容器连接网络失败: {}",
                    e
                )))
            }
        }
    }

    /// 获取 Docker 客户端实例
    pub fn get_docker_client(&self) -> &Docker {
        &self.docker
    }

    /// 获取容器网络信息
    pub async fn get_container_network_info(
        &self,
        container_id: &str,
    ) -> DockerResult<HashMap<String, String>> {
        use bollard::query_parameters::InspectContainerOptions;

        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("获取容器信息失败: {}", e)));

        let mut network_ips = HashMap::new();

        if let Some(network_settings) = inspect.unwrap().network_settings {
            if let Some(networks) = network_settings.networks {
                for (network_name, network_info) in networks {
                    if let Some(ip_address) = network_info.ip_address {
                        if !ip_address.is_empty() {
                            network_ips.insert(network_name, ip_address);
                        }
                    }
                }
            }
        }

        Ok(network_ips)
    }

    /// 检查容器健康状态
    async fn check_container_health(&self, container_id: &str) -> DockerResult<()> {
        use bollard::query_parameters::InspectContainerOptions;

        // 检查容器详细信息
        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("检查容器状态失败: {}", e)))?;

        // 检查容器状态
        if let Some(state) = inspect.state {
            let status = state.status;
            let exit_code = state.exit_code.unwrap_or(-1);

            match status {
                Some(bollard::models::ContainerStateStatusEnum::RUNNING) => {
                    info!("✅ 容器 {} 正在运行", container_id);
                    return Ok(());
                }
                Some(bollard::models::ContainerStateStatusEnum::EXITED) => {
                    let error_msg = state.error.as_deref().unwrap_or("未知错误");
                    error!(
                        "❌ 容器 {} 已退出 (退出码: {}): {}",
                        container_id, exit_code, error_msg
                    );
                    return Err(DockerError::ContainerStartError(format!(
                        "容器启动后立即退出: {} (退出码: {}), 错误: {}",
                        container_id, exit_code, error_msg
                    )));
                }
                Some(bollard::models::ContainerStateStatusEnum::CREATED) => {
                    warn!("⚠️ 容器 {} 已创建但未启动", container_id);
                    return Err(DockerError::ContainerStartError(format!(
                        "容器已创建但未启动: {}",
                        container_id
                    )));
                }
                Some(status) => {
                    let status_str = format!("{:?}", status);
                    error!("❌ 容器 {} 处于未知状态: {}", container_id, status_str);
                    return Err(DockerError::ContainerStartError(format!(
                        "容器处于未知状态: {} - {}",
                        container_id, status_str
                    )));
                }
                None => {
                    error!("❌ 容器 {} 状态为空", container_id);
                    return Err(DockerError::ContainerStartError(format!(
                        "容器状态为空: {}",
                        container_id
                    )));
                }
            }
        } else {
            error!("❌ 无法获取容器 {} 的状态信息", container_id);
            return Err(DockerError::ContainerStartError(format!(
                "无法获取容器状态信息: {}",
                container_id
            )));
        }
    }

    /// 根据模式列出 Docker 容器
    ///
    /// # Arguments
    /// * `pattern` - 容器名称模式，支持通配符（如 "rcoder-agent-*"）
    ///
    /// # Returns
    /// 返回匹配的容器信息列表
    pub async fn list_containers_with_pattern(
        &self,
        pattern: &str,
    ) -> DockerResult<Vec<ContainerSummary>> {
        info!("🔍 查找匹配模式的容器: pattern={}", pattern);

        // 使用 Docker API 列出所有容器（包括停止的）
        let options = Some(ListContainersOptions {
            all: true,
            ..Default::default()
        });

        let containers = self
            .docker
            .list_containers(options)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("获取容器列表失败: {}", e)))?;

        // 创建过滤器
        let filter = ContainerFilter::name_pattern(pattern);

        // 过滤容器
        let matched_containers: Vec<ContainerSummary> = containers
            .clone()
            .into_iter()
            .filter(|container| filter.matches(container))
            .collect();

        info!(
            "✅ 容器查找完成: 总数={}, 匹配数={}, pattern={}",
            containers.len(),
            matched_containers.len(),
            pattern
        );

        Ok(matched_containers)
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
        info!("🔥 开始批量清理容器: 数量={}", container_ids.len());

        let start_time = Instant::now();
        let mut result = CleanupResult::default();
        result.total_found = container_ids.len();

        for container_id in &container_ids {
            match self
                .stop_and_remove_single_container(container_id, &options)
                .await
            {
                Ok(_) => {
                    result.successfully_removed += 1;
                    result.removed_container_ids.push(container_id.clone());
                    info!("✅ 容器清理成功: {}", container_id);
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
                    error!("❌ 容器清理失败: {} - {}", container_id, e);
                }
            }
        }

        result.duration_ms = start_time.elapsed().as_millis() as u64;

        info!(
            "🧹 容器批量清理完成: 总数={}, 成功={}, 失败={}, 耗时={}ms",
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
        info!("🔄 正在清理容器: {}", container_id);

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
                    info!("⚠️ 容器 {} 正在运行，跳过删除（非强制模式）", container_id);
                    return Ok(());
                }

                if options.wait_for_graceful_stop {
                    info!("🛑 正在优雅停止容器: {}", container_id);
                    if let Err(e) = self
                        .graceful_stop_container(container_id, options.stop_timeout_seconds)
                        .await
                    {
                        warn!("优雅停止失败，强制停止: {} - {}", container_id, e);
                        // 强制停止
                        self.force_stop_container(container_id).await?;
                    }
                } else {
                    // 直接强制停止
                    self.force_stop_container(container_id).await?;
                }
            }
            Some(_) => {
                info!("📦 容器 {} 未运行，直接删除", container_id);
            }
            None => {
                warn!("⚠️ 无法获取容器 {} 状态，继续尝试删除", container_id);
            }
        }

        // 第三步：删除容器
        self.remove_single_container(container_id, options.remove_associated_volumes)
            .await?;

        info!("✅ 容器清理完成: {}", container_id);
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
            .map_err(|e| DockerError::ConnectionError(format!("获取容器信息失败: {}", e)))
    }

    /// 优雅停止容器
    async fn graceful_stop_container(
        &self,
        container_id: &str,
        timeout_seconds: u64,
    ) -> DockerResult<()> {
        let stop_options = Some(StopContainerOptions {
            t: Some((timeout_seconds as i32).try_into().unwrap()),
            signal: None::<String>,
        });

        self.docker
            .stop_container(container_id, stop_options)
            .await
            .map_err(|e| DockerError::ContainerStopError(format!("优雅停止容器失败: {}", e)))
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
            .map_err(|e| DockerError::ContainerStopError(format!("强制停止容器失败: {}", e)))
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
            .map_err(|e| DockerError::ContainerRemoveError(format!("删除容器失败: {}", e)))
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
        info!("🧹 开始模式匹配清理容器: pattern={:?}", pattern);

        // 第一步：查找匹配的容器
        let matched_containers = self.list_containers_with_pattern(pattern).await?;

        // 第二步：提取容器ID
        let container_ids: Vec<String> = matched_containers
            .iter()
            .filter_map(|container| container.id.as_ref())
            .cloned()
            .collect();

        info!(
            "🎯 找到 {} 个匹配的容器: pattern={}",
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
                let mut keys_to_remove = Vec::new();

                for entry in self.containers.iter() {
                    let (project_id, container_info) = entry.pair();
                    if container_info.container_id == *container_id {
                        keys_to_remove.push(project_id.clone());
                    }
                }

                for project_id in keys_to_remove {
                    self.containers.remove(&project_id);
                    info!(
                        "🧹 从内部映射中移除: project_id={}, container_id={}",
                        project_id, container_id
                    );
                }
            }
        }
    }

    /// 获取 RCoder 网络名称
    pub fn get_rcoder_network_name(&self) -> &'static str {
        RCODER_NETWORK_NAME
    }
}

impl std::fmt::Debug for DockerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerManager")
            .field("containers_count", &self.containers.len())
            .field("config", &self.config)
            .finish()
    }
}

/// 为了支持 futures Stream，需要导入 Next trait
use futures_util::stream::StreamExt;

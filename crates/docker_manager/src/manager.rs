use super::{
    CleanupOptions, CleanupResult, ContainerFilter, ContainerRemovalFailure, ContainerQueryResult,
    ContainerStatus, DockerContainerConfig, DockerContainerInfo, DockerError, DockerManagerConfig,
    DockerResult,
};
use crate::container_state_actor::{ContainerStateActor, ContainerStateHandle};
use anyhow::Result;
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    LogsOptions, RemoveContainerOptions, RestartContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::{
    API_DEFAULT_VERSION, Docker,
    models::{
        ContainerCreateBody, ContainerSummary, HostConfig, Mount, NetworkingConfig, PortBinding,
    },
};
use chrono::{DateTime, Utc};
use shared_types::ContainerBasicInfo;
use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Docker 容器管理器
pub struct DockerManager {
    /// Docker 客户端
    docker: Docker,
    /// 管理器配置
    config: DockerManagerConfig,
    /// 容器状态句柄（Actor 模式，无锁并发安全）
    containers: ContainerStateHandle,
    /// 主网络名称（动态检测或使用默认值）
    main_network_name: std::sync::Arc<tokio::sync::RwLock<String>>,
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

        // 🔍 动态检测主网络名称（必须成功）
        let main_network_name =
            match Self::detect_main_network_name_static(&docker, &config.network_base_name).await {
                Ok(network_name) => {
                    info!("✅ 检测到主网络名称: {}", network_name);
                    network_name
                }
                Err(e) => {
                    error!("❌ 无法检测主网络名称: {}", e);
                    return Err(e);
                }
            };

        // 🆕 创建容器状态 Actor 并启动
        let (actor, containers) = ContainerStateActor::new();
        tokio::spawn(actor.run());
        info!("✅ ContainerStateActor 已启动");

        let manager = Self {
            docker,
            config,
            containers,
            main_network_name: std::sync::Arc::new(tokio::sync::RwLock::new(main_network_name)),
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

        // 🔍 先检查 Docker API 中是否存在同名容器（无论运行状态）
        // 这是必要的，因为容器可能被外部 stop 但未删除，导致 409 Conflict
        if let Ok(Some((existing_container_id, existing_name, status, is_running))) =
            self.find_container_realtime(&container_name).await
        {
            warn!(
                "🔍 [CREATE] 发现同名容器: name={}, id={}, status={:?}, running={}",
                existing_name, existing_container_id, status, is_running
            );

            // 无论容器是运行还是停止状态，都先删除
            info!(
                "🗑️ [CREATE] 删除旧容器 {} ({})",
                existing_name, existing_container_id
            );
            if let Err(e) = self.stop_container_by_id(&existing_container_id).await {
                error!("❌ [CREATE] 删除旧容器失败: {}", e);
                // 继续尝试创建，Docker 会返回 409 错误
            } else {
                info!("✅ [CREATE] 旧容器删除成功");
            }
        }

        // 检查内存缓存并清理
        if let Some(existing) = self.containers.get(&config.project_id).await {
            warn!(
                "🧹 [CREATE] 清理内存缓存中的容器记录: project_id={}, container_name={}",
                config.project_id, existing.container_name
            );
            self.containers.remove(&config.project_id).await;
        }

        // 拉取镜像（如果本地不存在）
        self.ensure_image_exists(&config.image).await?;

        // 创建挂载点
        let mut mounts = Vec::new();

        // 只在 host_path 非空时添加主挂载点
        // 如果为空，表示完全依赖 extra_mounts（例如 ComputerAgentRunner）
        if !config.host_path.is_empty() {
            mounts.push(Mount {
                target: Some(config.container_path.clone()),
                source: Some(config.host_path.clone()),
                typ: Some(bollard::models::MountTypeEnum::BIND),
                read_only: Some(false),
                ..Default::default()
            });
            debug!(
                "📌 [DOCKER_MGR] 添加主挂载: {} -> {}",
                config.host_path, config.container_path
            );
        } else {
            debug!("📌 [DOCKER_MGR] 跳过主挂载，使用 extra_mounts 配置");
        }

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
        #[allow(unused_mut)]
        let mut env_vars: Vec<String> = config
            .env_vars
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // 🔧 eBPF 调试模式：启用自动火焰图生成、Grafana Alloy 持续剖析和 Off-CPU 阻塞分析
        #[cfg(feature = "ebpf-debug")]
        {
            // 通用监控配置（共享）
            env_vars.push("SAMPLE_DURATION=30".to_string());      // 采样时长（秒）
            env_vars.push("GENERATE_INTERVAL=60".to_string());    // 生成间隔（秒）
            env_vars.push("MAX_FLAMEFILES=50".to_string());        // 最大火焰图文件数
            env_vars.push("MAX_OFFCPU_FILES=50".to_string());     // 最大 Off-CPU 文件数

            // 自动火焰图生成
            env_vars.push("ENABLE_EBPF_AUTO_FLAMEGRAPH=true".to_string());

            // Grafana Alloy 配置（替代已废弃的 Pyroscope Agent）
            env_vars.push("ENABLE_ALLOY=true".to_string());
            env_vars.push("PYROSCOPE_URL=http://pyroscope:4040".to_string());

            // Off-CPU 阻塞分析配置
            env_vars.push("ENABLE_OFFCPUTIME=true".to_string());
            env_vars.push("OFFCPU_DURATION=30".to_string());      // Off-CPU 采样时长
            env_vars.push("OFFCPU_INTERVAL=60".to_string());      // Off-CPU 生成间隔（60秒，与系统调用监控一致）

            // 系统调用监控配置
            env_vars.push("ENABLE_SYSCALL_MONITOR=true".to_string());
        }

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

            // 🔧 容器权限配置：根据 feature 控制特权模式
            #[cfg(feature = "ebpf-debug")]
            // 调试模式：启用容器特权，允许使用 eBPF 工具
            // 通过 make dev-restart 启用此模式
            privileged: Some(true),

            #[cfg(not(feature = "ebpf-debug"))]
            // 生产模式：限制容器权限，提升安全性
            privileged: Some(false),

            #[cfg(feature = "ebpf-debug")]
            cap_add: Some(vec![
                "SYS_ADMIN".to_string(),    // eBPF 需要
                "NET_ADMIN".to_string(),    // 网络监控
                "SYS_PTRACE".to_string(),   // ptrace 追踪
            ]),

            #[cfg(not(feature = "ebpf-debug"))]
            cap_drop: Some(vec![
                "NET_RAW".to_string(),      // 禁止原始套接字（防止 ping、traceroute 等）
                "NET_ADMIN".to_string(),    // 禁止网络管理（防止修改路由表）
            ]),

            ..Default::default()
        };

        // 应用资源限制
        if let Some(ref limits) = config.resource_limits {
            host_config.memory = limits.memory_limit.map(|v| v as i64);
            host_config.memory_swap = limits.swap_limit.map(|v| v as i64);
            // CPU 限制需要通过 nano_cpus 设置 (1 CPU = 1e9 nano CPUs)
            if let Some(cpu_limit) = limits.cpu_limit {
                host_config.nano_cpus = Some((cpu_limit * 1e9) as i64);
            }
        }

        // 创建容器配置
        // 🎯 直接连接到主网络，所有容器共享同一网络以便互相通信
        let (networking_config, container_network_name) = if config.network_mode != "host" {
            let main_network = self.get_main_network_name().await;
            let network_name = config.network_name.as_ref().unwrap_or(&main_network);

            let mut endpoints = HashMap::new();
            endpoints.insert(
                network_name.clone(),
                bollard::models::EndpointSettings {
                    aliases: Some(vec![container_name.clone()]),
                    ..Default::default()
                },
            );

            info!(
                "🌐 [NETWORK] 容器 {} 连接到主网络: {}",
                container_name, network_name
            );

            (
                Some(NetworkingConfig {
                    endpoints_config: Some(endpoints),
                }),
                network_name.clone(),
            )
        } else {
            info!("🌐 [NETWORK] 容器 {} 使用 host 网络模式", container_name);
            (None, "host".to_string())
        };

        let mut container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            working_dir: Some(config.work_dir.clone()),
            env: Some(env_vars),
            host_config: Some(host_config),
            networking_config, // 🎯 直接指定网络配置
            tty: Some(true),
            open_stdin: Some(true),
            // 🔒 设置容器主机名和域名，便于识别和管理
            // 注意：这不能阻止容器访问内网 IP，只是设置容器的标识
            hostname: Some(format!(
                "agent-{}",
                &config.project_id[..8.min(config.project_id.len())]
            )),
            domainname: Some("rcoder.local".to_string()),
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

        // 等待容器启动完成
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // 检查容器状态，确保容器正在运行
        self.check_container_health(&container_id).await?;

        // 再次等待确保网络配置完成
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // 创建容器信息
        // 注意：由于容器间通过 Docker 内部网络通信，assigned_port 设为 0
        // 实际通信使用 container_ip:internal_port
        let container_info = DockerContainerInfo {
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            project_id: config.project_id.clone(),
            user_id: None,      // 将由调用方在 start_agent_container 中设置
            service_type: None, // 将由调用方在 start_agent_container 中设置
            image: config.image.clone(),
            status: ContainerStatus::Running,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            host_path: config.host_path.clone(),
            container_path: config.container_path.clone(),
            port_bindings: config.port_bindings.clone(),
            assigned_port: 0, // 内部网络通信，不需要宿主机端口
            health_status: None,
            service_health: None,                         // 初始无健康检查结果
            internal_port: 8080,                          // 默认内部端口
            network_name: container_network_name.clone(), // 记录使用的网络名称
        };

        // 保存到容器映射
        self.containers
            .insert(config.project_id.clone(), container_info.clone())
            .await;

        info!(
            "✅ 容器创建并启动成功: {} (ID: {}) - 已连接到网络 {}",
            container_name, container_id, container_network_name
        );

        Ok(container_info)
    }

    /// 通过容器ID停止容器
    pub async fn stop_container_by_id(&self, container_id: &str) -> DockerResult<()> {
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
            "快速销毁容器: {} (超时: {}秒)",
            container_id, timeout_seconds
        );

        // 🚀 直接使用 force remove，无需先 stop
        // force: true 会自动停止运行中的容器
        // 这样可以避免 "removal already in progress" 的竞态问题
        let remove_options = Some(RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        });

        // 不容忍任何错误，直接返回让调用者处理
        // 这样可以避免掩盖潜在问题（如 404 可能有多种含义，409 的字符串匹配不可靠）
        self.docker
            .remove_container(container_id, remove_options)
            .await
            .map_err(|e| {
                warn!("删除容器 {} 失败: {}", container_id, e);
                DockerError::BollardError(e)
            })?;

        info!("✅ 容器销毁成功: {}", container_id);

        // 从映射中移除（如果存在）- 通过遍历查找 project_id
        for info in self.containers.list().await {
            if info.container_id == container_id {
                self.containers.remove(&info.project_id).await;
                break;
            }
        }

        Ok(())
    }

    /// 停止并删除容器
    pub async fn stop_container(&self, project_id: &str) -> DockerResult<()> {
        info!("停止容器，项目ID: {}", project_id);

        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
        } else {
            warn!("项目 {} 没有找到对应的容器", project_id);
            return Ok(());
        };

        // 调用通过ID停止的方法
        self.stop_container_by_id(&container_info.container_id)
            .await?;

        // 从映射中移除
        self.containers.remove(project_id).await;

        Ok(())
    }

    /// 通过 project_id 从缓存中获取容器信息
    ///
    /// 从内存缓存中查询容器信息，速度快但可能不是最新状态。
    ///
    /// # 参数
    /// * `project_id` - 项目 ID（RCoder 模式的主键）
    ///
    /// # 返回
    /// * 如果缓存中存在，返回 `Some(DockerContainerInfo)`
    /// * 如果缓存中不存在，返回 `None`
    ///
    /// # 注意
    /// - 此方法从缓存查询，容器信息可能不是最新的
    /// - 如果需要最新的容器状态，请使用 [`get_container_info_by_name`](Self::get_container_info_by_name)
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

    /// 通过多种方式查找容器：project_id 或容器名称
    ///
    /// # ⚠️ 已废弃
    ///
    /// 此方法返回的容器信息可能包含过期的 `container_id`。
    ///
    /// **推荐使用**：
    /// - [`find_container_realtime`](Self::find_container_realtime) - 实时查询 Docker API，获取最新的容器信息和 ID
    ///
    /// **问题**：
    /// - 返回的 `container_id` 可能是缓存中的旧值
    /// - 容器重启后 ID 会变化，导致使用旧 ID 操作失败（404 错误）
    ///
    /// **迁移指南**：
    /// ```text
    /// // ❌ 旧方式（可能使用过期的 container_id）
    /// if let Some(info) = docker_manager.find_container_by_identifier("container_name").await {
    ///     docker_manager.stop_container_by_id(&info.container_id).await?;
    /// }
    ///
    /// // ✅ 新方式（获取最新的 container_id）
    /// if let Ok(Some((container_id, _, _, _))) =
    ///     docker_manager.find_container_realtime("container_name").await
    /// {
    ///     docker_manager.stop_container_by_id(&container_id).await?;
    /// }
    /// ```
    #[deprecated(
        since = "0.1.0",
        note = "返回的 container_id 可能过期。请使用 find_container_realtime() 获取最新的容器信息"
    )]
    pub async fn find_container_by_identifier(
        &self,
        identifier: &str,
    ) -> Option<DockerContainerInfo> {
        // 1. 首先尝试通过 project_id 查找
        if let Some(info) = self.containers.get(identifier).await {
            return Some(info);
        }

        // 2. 如果没找到，尝试通过容器名称查找
        for info in self.containers.list().await {
            if info.container_name == identifier {
                return Some(info);
            }
        }

        // 3. 如果还没找到，尝试通过 Docker API 直接查找容器（适用于容器存在但映射缺失的情况）
        let options = Some(ListContainersOptions {
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

                            // 🛡️ 从容器信息中获取真实的创建时间
                            // 使用统一的时间戳解析函数
                            let created_at = if let Some(created_timestamp) = container.created {
                                // list_containers API 返回的是 Unix 秒时间戳
                                Self::parse_unix_timestamp(
                                    created_timestamp,
                                    &format!("container {}", clean_name)
                                )
                                .unwrap_or_else(|e| {
                                    warn!("⚠️ 解析容器创建时间失败: {}, 使用当前时间", e);
                                    Utc::now()
                                })
                            } else {
                                warn!(
                                    "⚠️ 容器缺少创建时间信息，使用当前时间作为备用: container_id={}",
                                    container_id
                                );
                                Utc::now()
                            };

                            // 创建一个临时的容器信息，用于销毁
                            return Some(DockerContainerInfo {
                                container_id,
                                container_name: clean_name.to_string(),
                                project_id: "unknown".to_string(), // 我们无法直接知道 project_id
                                user_id: None,
                                service_type: None,
                                image: container.image.unwrap_or_default(),
                                status: ContainerStatus::Unknown(
                                    "found_via_docker_api".to_string(),
                                ),
                                created_at,
                                started_at: None,
                                host_path: String::new(),
                                container_path: String::new(),
                                port_bindings: std::collections::HashMap::new(),
                                assigned_port: 0,
                                health_status: None,
                                service_health: None,
                                internal_port: 0,
                                network_name: "unknown".to_string(), // 临时容器信息，网络名称未知
                            });
                        }
                    }
                }
            }
        }

        None
    }

    /// 获取所有容器信息
    pub async fn list_containers(&self) -> Vec<DockerContainerInfo> {
        self.containers.list().await
    }

    /// 检查指定ID的容器是否正在运行
    pub async fn is_container_running(&self, container_id: &str) -> DockerResult<bool> {
        match self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                if let Some(state) = details.state
                    && let Some(status) = state.status {
                        return Ok(status == bollard::models::ContainerStateStatusEnum::RUNNING);
                    }
                Ok(false)
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在，安全地返回 false
                Ok(false)
            }
            Err(e) => {
                // 其他类型的错误，作为错误返回
                Err(DockerError::BollardError(e))
            }
        }
    }

    /// 实时查询容器状态（直接查询 Docker API，不使用缓存）
    ///
    /// 与 `find_container_by_identifier` 不同，此方法跳过内存缓存，
    /// 直接查询 Docker API 获取最新的容器状态。
    ///
    /// # 参数
    /// * `identifier` - 容器名称或容器 ID
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some((container_id, container_name, status, is_running))`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_container_realtime(
        &self,
        identifier: &str,
    ) -> DockerResult<Option<(String, String, ContainerStatus, bool)>> {
        debug!("🔍 [REALTIME] 实时查询容器状态: identifier={}", identifier);

        match self
            .docker
            .inspect_container(identifier, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
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

                info!(
                    "✅ [REALTIME] 容器状态查询成功: id={}, name={}, status={:?}, running={}",
                    container_id, container_name, status, is_running
                );

                Ok(Some((container_id, container_name, status, is_running)))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在
                debug!("📭 [REALTIME] 容器不存在: identifier={}", identifier);
                Ok(None)
            }
            Err(e) => {
                // 其他类型的错误
                error!(
                    "❌ [REALTIME] 查询容器状态失败: identifier={}, error={}",
                    identifier, e
                );
                Err(DockerError::BollardError(e))
            }
        }
    }

    /// 通过容器名称获取容器创建时间
    ///
    /// 直接查询 Docker API 获取容器的创建时间，不使用缓存。
    /// 主要用于容器保护期检查，确保刚创建的容器不会被误清理。
    ///
    /// # 参数
    /// * `container_name` - 容器名称
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(created_time)`
    /// * 如果容器不存在，返回 `None`
    /// * 如果解析时间失败，返回错误
    ///
    /// # 示例
    /// ```ignore
    /// let created = docker_manager
    ///     .get_container_creation_time_by_name("rcoder-agent-123")
    ///     .await?;
    /// if let Some(time) = created {
    ///     let age = Utc::now().signed_duration_since(time);
    ///     if age.num_seconds() < protection_seconds {
    ///         // 在保护期内，跳过清理
    ///     }
    /// }
    /// ```
    pub async fn get_container_creation_time_by_name(
        &self,
        container_name: &str,
    ) -> DockerResult<Option<DateTime<Utc>>> {
        debug!(
            "🔍 [DOCKER_MGR] 查询容器创建时间: container_name={}",
            container_name
        );

        match self
            .docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                if let Some(ref created_str) = details.created {
                    match Self::parse_rfc3339_timestamp(created_str, &format!("container {}", container_name)) {
                        Ok(created_time_utc) => {
                            debug!(
                                "✅ [DOCKER_MGR] 容器创建时间: container_name={}, created={}",
                                container_name, created_time_utc
                            );
                            Ok(Some(created_time_utc))
                        }
                        Err(e) => {
                            error!(
                                "❌ [DOCKER_MGR] 解析容器创建时间失败: container_name={}, error={}",
                                container_name, e
                            );
                            Err(DockerError::InvalidTimestamp(e))
                        }
                    }
                } else {
                    warn!(
                        "⚠️ [DOCKER_MGR] 容器创建时间字段为空: container_name={}",
                        container_name
                    );
                    Ok(None)
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在
                debug!(
                    "📭 [DOCKER_MGR] 容器不存在: container_name={}",
                    container_name
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "❌ [DOCKER_MGR] 查询容器信息失败: container_name={}, error={}",
                    container_name, e
                );
                Err(DockerError::BollardError(e))
            }
        }
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
    fn parse_rfc3339_timestamp(timestamp_str: &str, context: &str) -> Result<DateTime<Utc>, String> {
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
    fn parse_unix_timestamp(timestamp_secs: i64, context: &str) -> Result<DateTime<Utc>, String> {
        DateTime::from_timestamp(timestamp_secs, 0)
            .ok_or_else(|| {
                format!(
                    "Failed to parse Unix timestamp for {}: {} (out of range)",
                    context, timestamp_secs
                )
            })
    }

    /// 通过容器名称从 Docker API 获取完整容器信息
    ///
    /// 直接查询 Docker API 获取最新的容器信息，不使用缓存。
    /// 返回完整的 DockerContainerInfo 结构，包含所有容器元数据。
    ///
    /// # 参数
    /// * `container_name` - 容器名称
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(DockerContainerInfo)`
    /// * 如果容器不存在，返回 `None`
    ///
    /// # 示例
    /// ```ignore
    /// if let Some(info) = docker_manager
    ///     .get_container_info_by_name("rcoder-agent-123")
    ///     .await?
    /// {
    ///     println!("容器状态: {:?}, 创建时间: {}", info.status, info.created_at);
    /// }
    /// ```
    ///
    /// # 与其他方法的对比
    /// - [`get_container_info`](Self::get_container_info): 通过 project_id 从缓存查询（快速但可能过期）
    /// - [`find_container_realtime`](Self::find_container_realtime): 返回简化信息（只有 id/name/status）
    /// - **此方法**: 通过 name 查询完整信息（最新数据）
    pub async fn get_container_info_by_name(
        &self,
        container_name: &str,
    ) -> DockerResult<Option<DockerContainerInfo>> {
        debug!(
            "🔍 [DOCKER_MGR] 通过容器名称查询完整信息: container_name={}",
            container_name
        );

        match self
            .docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                // 解析容器 ID
                let container_id = details.id.ok_or_else(|| {
                    DockerError::ConfigurationError("容器 ID 为空".to_string())
                })?;

                // 解析容器名称（去除前导斜杠）
                let name = details
                    .name
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| container_name.to_string());

                // 解析状态和启动时间
                let (status, started_at) = if let Some(state) = details.state {
                    let status_str = state
                        .status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    // 使用统一的时间解析函数
                    let started = state.started_at.and_then(|s| {
                        Self::parse_rfc3339_timestamp(&s, "started_at").ok()
                    });

                    (ContainerStatus::from(status_str), started)
                } else {
                    (ContainerStatus::Unknown("no state".to_string()), None)
                };

                // 解析创建时间 - 使用统一的时间解析函数
                let created_at = details
                    .created
                    .ok_or_else(|| {
                        DockerError::InvalidTimestamp("容器缺少 created 字段".to_string())
                    })
                    .and_then(|s| {
                        Self::parse_rfc3339_timestamp(&s, "created")
                            .map_err(|e| DockerError::InvalidTimestamp(e))
                    })?;

                // 解析镜像
                let image = details.config.as_ref()
                    .and_then(|c| c.image.clone())
                    .unwrap_or_default();

                // 解析挂载信息（查找工作目录绑定）
                let (host_path, container_path) = details
                    .mounts
                    .as_ref()
                    .and_then(|mounts| {
                        mounts.iter().find(|m: &&bollard::secret::MountPoint| {
                            matches!(m.typ, Some(bollard::secret::MountPointTypeEnum::BIND))
                        })
                    })
                    .and_then(|mount| {
                        let source = mount.source.clone()?;
                        let destination = mount.destination.clone()?;
                        Some((source, destination))
                    })
                    .unwrap_or_else(|| (String::new(), String::new()));

                // 解析网络和端口信息
                let (network_name, port_bindings, assigned_port) = if let Some(ref network_settings) = details.network_settings {
                    // 解析网络名称
                    let net_name = network_settings
                        .networks
                        .as_ref()
                        .and_then(|networks| networks.keys().next().cloned())
                        .unwrap_or_default();

                    // 解析端口映射
                    let mut ports = HashMap::new();
                    let mut assigned = 0u16;

                    if let Some(ref port_map) = network_settings.ports {
                        for (container_port, host_bindings) in port_map {
                            if let Some(bindings) = host_bindings {
                                for binding in bindings {
                                    if let Some(ref host_port) = binding.host_port {
                                        ports.insert(
                                            container_port.clone(),
                                            host_port.clone(),
                                        );
                                        // 尝试解析为数字端口
                                        if assigned == 0 {
                                            if let Ok(port) = host_port.parse::<u16>() {
                                                assigned = port;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    (net_name, ports, assigned)
                } else {
                    (String::new(), HashMap::new(), 0u16)
                };

                // 从 Labels 中提取 project_id, user_id, service_type
                let labels = details.config.as_ref().and_then(|c| c.labels.as_ref());
                let project_id = labels
                    .and_then(|l| l.get("project_id"))
                    .cloned()
                    .unwrap_or_default();
                let user_id = labels.and_then(|l| l.get("user_id")).cloned();
                let service_type = labels
                    .and_then(|l| l.get("service_type"))
                    .and_then(|s| s.parse().ok()); // 使用 FromStr trait

                // 内部端口（默认）
                let internal_port = match service_type {
                    Some(shared_types::ServiceType::RCoder) => shared_types::GRPC_DEFAULT_PORT,
                    Some(shared_types::ServiceType::ComputerAgentRunner) => shared_types::HTTP_DEFAULT_PORT,
                    None => shared_types::GRPC_DEFAULT_PORT,
                };

                let info = DockerContainerInfo {
                    container_id,
                    container_name: name,
                    project_id,
                    user_id,
                    service_type,
                    image,
                    status,
                    created_at,
                    started_at,
                    host_path,
                    container_path,
                    port_bindings,
                    assigned_port,
                    health_status: None,
                    service_health: None,
                    internal_port,
                    network_name,
                };

                debug!(
                    "✅ [DOCKER_MGR] 容器信息查询成功: name={}, id={}, status={:?}",
                    info.container_name, info.container_id, info.status
                );

                Ok(Some(info))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // 容器不存在
                debug!(
                    "📭 [DOCKER_MGR] 容器不存在: container_name={}",
                    container_name
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "❌ [DOCKER_MGR] 查询容器信息失败: container_name={}, error={}",
                    container_name, e
                );
                Err(DockerError::BollardError(e))
            }
        }
    }

    /// 启动 Agent 容器（全流程封装）
    ///
    /// 替代 rcoder 层的复杂编排逻辑
    pub async fn start_agent_container(
        &self,
        project_id: Option<&str>, // 容器标识符（project_id），用于清理旧容器，可选
        user_id: Option<&str>,    // Computer Agent Runner 中使用，其他情况为 None
        host_workspace_path: &str,
        service_type: shared_types::ServiceType,
        request_resource_limits: Option<shared_types::ServiceResourceLimits>,
    ) -> DockerResult<ContainerBasicInfo> {
        info!(
            "启动 Agent 容器: project_id={:?}, user_id={:?}, type={:?}, host_path={}",
            project_id, user_id, service_type, host_workspace_path
        );

        // 1. 在宿主机上预创建工作目录
        // 1. 检查工作目录是否已存在（通过绑定挂载，容器内创建会自动同步）
        debug!("🔍 [DOCKER_MGR] 检查工作目录: {}", host_workspace_path);
        // 绑定挂载机制：容器内创建目录会自动同步到宿主机
        // 所以这里不需要额外创建目录

        // 2. 清理旧容器（如果提供了 project_id）
        if let Some(id) = project_id
            && let Some(existing) = self.get_container_info(id).await {
                warn!("发现旧容器 {}，正在停止...", existing.container_name);
                self.stop_container(id).await?;
            }

        // 2. 获取配置和镜像
        let service_config = self.get_service_config(&service_type).await?;
        let image = self.select_image(&service_type, None).await?;

        // 3. 准备配置
        use crate::container_builder::ContainerConfigBuilder;

        // 确定用于构建容器配置的主 ID
        // 标准 RCoder 使用 project_id，Computer Agent Runner 使用 user_id
        let container_id = if let Some(uid) = user_id {
            // Computer Agent Runner 使用 user_id
            uid
        } else if let Some(pid) = project_id {
            // 标准 RCoder 使用 project_id
            pid
        } else {
            // 错误：至少需要提供 project_id 或 user_id 其中一个
            return Err(DockerError::ConfigurationError(
                "必须提供 project_id 或 user_id 中的至少一个".to_string(),
            ));
        };

        // 解析容器内工作目录路径
        let mut variables = std::collections::HashMap::new();
        // 根据服务类型设置相应的变量
        if let Some(pid) = project_id {
            variables.insert("project_id".to_string(), pid.to_string());
        }
        if let Some(uid) = user_id {
            variables.insert("user_id".to_string(), uid.to_string());
        }
        variables.insert("service_type".to_string(), service_type.to_string());
        let container_work_path = service_config.resolve_container_path(&variables);

        // 构建基础配置
        let mut builder = ContainerConfigBuilder::new(container_id)
            .image(image)
            .name_prefix(service_type.container_prefix())
            .work_dir(service_config.work_dir.clone())
            .network_mode(service_config.network_mode.clone())
            .auto_remove(true);

        // 只在 host_workspace_path 非空时添加主挂载点
        // 如果为空，表示完全依赖 mounts 配置（例如 ComputerAgentRunner）
        if !host_workspace_path.is_empty() {
            builder = builder
                .host_path(host_workspace_path.to_string())
                .container_path(container_work_path.clone());

            debug!(
                "📌 [DOCKER_MANAGER] 主挂载: {} -> {}",
                host_workspace_path, container_work_path
            );
        } else {
            debug!("📌 [DOCKER_MANAGER] 跳过主挂载，使用 mounts 配置管理所有挂载点");
        }

        // 应用资源限制
        let limits = service_config.resource_limits;

        // 合并资源限制：请求级别覆盖服务级别
        let final_resource_limits = match request_resource_limits {
            Some(request_limits) => {
                // 再次验证（防御性编程）
                request_limits.validate().map_err(|e| {
                    DockerError::ConfigurationError(format!("Invalid resource limits: {}", e))
                })?;

                // 合并配置
                limits.merge_with(&request_limits)
            }
            None => limits,
        };

        builder = builder.resource_limits(crate::types::ResourceLimits {
            memory_limit: final_resource_limits.memory_limit,
            cpu_limit: final_resource_limits.cpu_limit,
            swap_limit: final_resource_limits.swap_limit,
        });

        // 添加环境变量
        // 根据服务类型设置相应的环境变量
        if let Some(pid) = project_id {
            builder = builder.env("PROJECT_ID", pid);
        }
        if let Some(uid) = user_id {
            builder = builder.env("USER_ID", uid);
        }
        // 处理其他环境变量中的模板
        for (key, value) in &service_config.environment {
            let mut processed_value = value.clone();
            if let Some(pid) = project_id {
                processed_value = processed_value.replace("{project_id}", pid);
            }
            if let Some(uid) = user_id {
                processed_value = processed_value.replace("{user_id}", uid);
            }
            builder = builder.env(key, &processed_value);
        }

        // 🔒 安全配置：将 API 地址指向本地 Pingora 代理
        // 使用 {SERVICE_UUID} 占位符，UUID 将由 agent_runner 动态注入
        // 这样 Agent 看到的是本地地址，真实密钥由 Pingora 注入
        builder = builder.env(
            "MODEL_PROVIDER_BASE_URL",
            "http://localhost:8088/api/{SERVICE_UUID}",
        );
        builder = builder.env("MODEL_PROVIDER_API_KEY", "sk-placeholder");

        // 注意：子容器以 root 用户运行，不再需要 UID/GID 匹配

        // 设置网络
        let network_name = self.get_main_network_name().await;
        builder = builder.network_name(network_name);

        // 设置命令
        builder = builder.command(service_config.command);
        if let Some(entry) = service_config.entrypoint {
            builder = builder.entrypoint(entry);
        }

        // 🎯 处理配置文件中的挂载点 (service_config.mounts)
        let container_name = format!("{}-{}", service_type.container_prefix(), container_id);
        let timestamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
        let log_dir_name = format!("{}-{}", container_name, timestamp);

        // 基础变量集
        let mut base_variables = variables.clone();
        base_variables.insert("container_name".to_string(), container_name.clone());
        base_variables.insert("timestamp".to_string(), timestamp.clone());
        base_variables.insert("log_dir_name".to_string(), log_dir_name.clone());

        // 缓存已解析的路径，避免重复解析
        let mut resolved_paths_cache: std::collections::HashMap<String, std::path::PathBuf> =
            std::collections::HashMap::new();

        // 添加配置文件中定义的挂载点
        for mount_config in &service_config.mounts {
            let mut mount_variables = base_variables.clone();

            // 如果配置了 resolve_from，解析动态路径
            if let Some(ref resolve_from_path) = mount_config.resolve_from {
                // 检查缓存（只缓存基础路径解析结果）
                let resolved_base =
                    if let Some(cached) = resolved_paths_cache.get(resolve_from_path) {
                        Some(cached.clone())
                    } else {
                        // 解析 resolve_from 路径到宿主机基础路径
                        match crate::path::resolve_container_path_to_host(std::path::Path::new(
                            resolve_from_path,
                        ))
                        .await
                        {
                            Ok(host_base_path) => {
                                info!(
                                    "📁 [DOCKER_MGR] 从 {} 解析到宿主机路径: {}",
                                    resolve_from_path,
                                    host_base_path.display()
                                );
                                // 缓存基础路径解析结果
                                resolved_paths_cache
                                    .insert(resolve_from_path.clone(), host_base_path.clone());
                                Some(host_base_path)
                            }
                            Err(e) => {
                                warn!(
                                    "⚠️ [DOCKER_MGR] 无法解析路径 (resolve_from: {}): {}",
                                    resolve_from_path, e
                                );
                                None
                            }
                        }
                    };

                // 添加解析后的基础路径变量
                if let Some(resolved_path) = resolved_base {
                    mount_variables.insert(
                        "resolved_path".to_string(),
                        resolved_path.to_string_lossy().to_string(),
                    );
                } else {
                    // 如果解析失败，跳过此挂载点
                    warn!(
                        "⚠️ [DOCKER_MGR] 跳过挂载点 (无法解析 resolve_from): {}",
                        mount_config.container_path
                    );
                    continue;
                }
            }

            // 解析宿主机路径中的变量
            let resolved_host_path = mount_config.resolve_host_path(&mount_variables);

            // 检查是否还有未替换的变量（如 {logs_host_path} 等）
            if resolved_host_path.contains('{') && resolved_host_path.contains('}') {
                warn!(
                    "⚠️ [DOCKER_MGR] 跳过挂载点 (host_path 包含未解析的变量): {}",
                    resolved_host_path
                );
                continue;
            }

            // 使用 PathBuf 规范化路径（消除多余的斜杠）
            let resolved_host_path = std::path::PathBuf::from(&resolved_host_path)
                .components()
                .collect::<std::path::PathBuf>()
                .to_string_lossy()
                .to_string();

            // 解析容器路径中的变量
            let mut resolved_container_path = mount_config.container_path.clone();
            for (key, value) in &mount_variables {
                resolved_container_path =
                    resolved_container_path.replace(&format!("{{{}}}", key), value);
            }

            info!(
                "📁 [DOCKER_MGR] 添加挂载点: {} -> {} (read_only: {})",
                resolved_host_path, resolved_container_path, mount_config.read_only
            );

            // 确保目录存在（仅对非只读挂载创建目录）
            // 重要：Docker bind mount 要求宿主机路径必须存在
            //
            // 由于代码运行在容器内，无法直接访问宿主机路径（如 /Volumes/soddygo/...）
            // 必须使用容器内通过 volume 挂载可访问的路径来创建目录
            //
            // 策略：必须配置 resolve_from 才能正确创建目录
            // 使用 resolve_from + 相对路径 来创建目录
            if !mount_config.read_only {
                if let Some(ref resolve_from_path) = mount_config.resolve_from {
                    // 从 host_path 模板中提取相对路径部分
                    // host_path 格式通常是 "{resolved_path}/{variable}"
                    // 我们需要取 {resolved_path} 之后的部分，并拼接到 resolve_from 上
                    let host_path_template = &mount_config.host_path;
                    let relative_part = if host_path_template.starts_with("{resolved_path}") {
                        // 提取 {resolved_path} 之后的模板部分并替换变量
                        let suffix = host_path_template
                            .strip_prefix("{resolved_path}")
                            .unwrap_or("");
                        let mut resolved_suffix = suffix.to_string();
                        for (key, value) in &mount_variables {
                            resolved_suffix =
                                resolved_suffix.replace(&format!("{{{}}}", key), value);
                        }
                        resolved_suffix
                    } else {
                        // 如果不是以 {resolved_path} 开头，尝试计算相对路径
                        // 这种情况不太常见，使用空字符串作为后备
                        String::new()
                    };

                    // 构建容器内可访问的路径
                    let create_path = format!("{}{}", resolve_from_path, relative_part);
                    debug!(
                        "📁 [DOCKER_MGR] 使用容器内路径创建目录: {} (resolve_from: {}, relative: {})",
                        create_path, resolve_from_path, relative_part
                    );

                    if let Err(e) = std::fs::create_dir_all(&create_path) {
                        warn!("⚠️ [DOCKER_MGR] 创建挂载目录失败: {} - {}", create_path, e);
                    } else {
                        info!("✅ [DOCKER_MGR] 目录创建成功: {}", create_path);
                    }
                } else {
                    // 没有配置 resolve_from，无法在容器内创建宿主机路径
                    // 假设目录已存在，跳过创建
                    info!(
                        "📁 [DOCKER_MGR] 跳过目录创建 (未配置 resolve_from): {}",
                        resolved_host_path
                    );
                }
            }

            builder = builder.add_mount(crate::MountPoint {
                host_path: resolved_host_path,
                container_path: resolved_container_path,
                read_only: mount_config.read_only,
            });

            // 如果是日志挂载，添加环境变量
            if mount_config.container_path.contains("container-logs") {
                builder = builder.env("CONTAINER_LOGS_DIR", &mount_config.container_path);
                builder = builder.env("CONTAINER_LOG_NAME", &log_dir_name);
            }
        }

        // 4. 创建并启动
        let config = builder
            .build()
            .map_err(|e| DockerError::ContainerCreationError(e.to_string()))?;

        self.create_container(config).await?;

        // 🆕 更新容器映射中的 user_id 和 service_type
        if let Some(mut info) = self.containers.get(container_id).await {
            info.user_id = user_id.map(|s| s.to_string());
            info.service_type = Some(service_type.clone());
            debug!(
                "📝 [DOCKER_MGR] 更新容器元数据: container_id={}, user_id={:?}, service_type={:?}",
                container_id, info.user_id, info.service_type
            );
            self.containers.insert(container_id.to_string(), info).await;
        }

        // 5. 等待就绪并返回信息
        let info = self.get_agent_info(container_id).await?.ok_or_else(|| {
            DockerError::ContainerStartError("容器启动后无法获取信息".to_string())
        })?;

        // 健康检查
        crate::health::wait_for_service_ready(&info.service_url)
            .await
            .map_err(|e| DockerError::ContainerStartError(format!("健康检查失败: {}", e)))?;

        info!("✅ Agent 容器启动并就绪: {}", info.service_url);
        Ok(info)
    }

    /// 查找项目容器（RCoder 模式专用）
    ///
    /// 根据 project_id 和 service_type 查找容器：
    /// - 容器命名规则：`{prefix}-{project_id}`
    /// - RCoder 模式前缀：`rcoder-agent`
    ///
    /// # 策略
    /// 1. 查找内部 Map (project_id)
    /// 2. 实时查询 Docker API (构造容器名称)
    ///
    /// # 返回
    /// * `Ok(Some(ContainerQueryResult))` - 容器存在
    /// * `Ok(None)` - 容器不存在
    /// * `Err(...)` - 查询出错
    pub async fn find_project_container(
        &self,
        project_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        // 1. 查 Map (如果存在且运行中，直接返回)
        if let Some(info) = self.containers.get(project_id).await {
            return Ok(Some(ContainerQueryResult::new(
                info.container_id.clone(),
                info.container_name.clone(),
                info.status.clone(),
                matches!(info.status, ContainerStatus::Running),
            )));
        }

        // 2. 实时查询 Docker API (构造名称)
        let prefix = service_type.container_prefix();
        let expected_container_name = format!("{}-{}", prefix, project_id);

        match self.find_container_realtime(&expected_container_name).await? {
            Some((container_id, container_name, status, is_running)) => {
                Ok(Some(ContainerQueryResult::new(
                    container_id,
                    container_name,
                    status,
                    is_running,
                )))
            }
            None => Ok(None),
        }
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
                // 检查是否是容器不存在的错误
                let error_str = e.to_string();
                if error_str.contains("No such container") || error_str.contains("404") {
                    // 容器已被外部删除，清理内存映射并返回 None
                    // 这样上层调用者可以重新创建容器
                    warn!(
                        "⚠️ [GET_AGENT_INFO] 容器已被外部删除，清理内存映射: project_id={}, container_id={}",
                        project_id, container_info.container_id
                    );
                    self.containers.remove(project_id).await;
                    return Ok(None);
                }
                // 其他错误正常传播
                return Err(e);
            }
        };

        let container_ip = network_ips
            .get(&network_name)
            .cloned()
            .or_else(|| network_ips.values().next().cloned())
            .ok_or_else(|| DockerError::ConnectionError("容器未连接到任何网络".to_string()))?;

        // 3. 构建服务 URL (Agent 内部默认监听 8086)
        let server_url = format!("http://{}:8086", container_ip);

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
                warn!("获取容器网络信息失败: {}", e);
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

    /// 获取用户容器信息（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID，用作容器标识符
    ///
    /// # 说明
    /// - ComputerAgentRunner 模式下，一个用户对应一个容器
    /// - 容器命名规则：`computer-agent-runner-{user_id}`
    /// - 容器内可以运行多个 project_id 的 Agent 实例
    ///
    /// # 返回
    /// 容器信息（如果存在），否则返回 None
    pub async fn get_user_container_info(
        &self,
        user_id: &str,
    ) -> DockerResult<Option<ContainerBasicInfo>> {
        // 内部调用 get_agent_info，但参数名更清晰
        self.get_agent_info(user_id).await
    }

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
            )));
        }

        // 2. 实时查询 Docker API (构造名称)
        let prefix = service_type.container_prefix();
        let expected_container_name = format!("{}-{}", prefix, user_id);

        match self.find_container_realtime(&expected_container_name).await? {
            Some((container_id, container_name, status, is_running)) => {
                Ok(Some(ContainerQueryResult::new(
                    container_id,
                    container_name,
                    status,
                    is_running,
                )))
            }
            None => Ok(None),
        }
    }

    /// 通过用户 ID 获取容器 ID（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    ///
    /// # 返回
    /// 容器 ID（如果存在），否则返回 None
    pub async fn get_user_container_id(&self, user_id: &str) -> DockerResult<Option<String>> {
        // 从容器信息中获取 container_id
        Ok(self
            .get_container_info(user_id)
            .await
            .map(|info| info.container_id))
    }

    /// 检查用户容器是否存在（ComputerAgentRunner 模式专用）
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID
    ///
    /// # 返回
    /// true 如果容器存在且运行中，否则返回 false
    pub async fn is_user_container_running(&self, user_id: &str) -> bool {
        match self
            .find_user_container(user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .await
        {
            Ok(Some(result)) => result.is_running,
            _ => false,
        }
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
    /// 返回元组 (已检查数量, 已移除数量)
    pub async fn sync_all_container_states(&self) -> DockerResult<(u32, u32)> {
        // 获取所有 project_id 的快照
        let project_ids: Vec<String> = self.containers.keys().await;

        if project_ids.is_empty() {
            return Ok((0, 0));
        }

        let total = project_ids.len() as u32;
        let mut removed_count = 0u32;
        let mut health_checked_count = 0u32;

        // 创建健康检查器（复用同一个实例）
        let health_checker = crate::health::ServiceHealthChecker::new();

        for project_id in project_ids {
            match self.update_container_status(&project_id).await {
                Ok(None) => {
                    // 容器不存在，已从缓存中移除
                    removed_count += 1;
                    info!(
                        "🧹 [SYNC] 容器已从缓存移除（Docker 中不存在）: project_id={}",
                        project_id
                    );
                }
                Ok(Some(status)) => {
                    // 🆕 对运行中的容器执行服务健康检查
                    if matches!(status, ContainerStatus::Running) {
                        // Actor 模式：直接异步获取，无锁
                        let container_info = self.containers.get(&project_id).await;
                        let Some(container_info) = container_info else {
                            continue;
                        };

                        // 获取容器 IP（锁已释放，可以安全 await）
                        if let Ok(network_ips) = self
                            .get_container_network_info(&container_info.container_id)
                            .await
                        {
                            // 优先使用主网络 IP，否则使用第一个可用 IP
                            let network_name = self.get_main_network_name().await;
                            let container_ip = network_ips
                                .get(&network_name)
                                .or_else(|| network_ips.values().next());

                            if let Some(ip) = container_ip {
                                // 获取之前的失败次数
                                let previous_failures = container_info
                                    .service_health
                                    .as_ref()
                                    .map(|h| h.consecutive_failures)
                                    .unwrap_or(0);

                                // 执行健康检查
                                let health_status =
                                    health_checker.check_service(ip, previous_failures).await;

                                // 更新缓存（这里获取新的写锁，不会死锁）
                                let mut updated_info = container_info.clone();
                                updated_info.service_health = Some(health_status.clone());
                                self.containers
                                    .insert(project_id.clone(), updated_info)
                                    .await;

                                health_checked_count += 1;

                                if health_status.is_fully_healthy() {
                                    debug!(
                                        "✅ [SYNC] 服务健康: container_id={}, service_type={:?}",
                                        project_id, container_info.service_type
                                    );
                                } else {
                                    warn!(
                                        "⚠️ [SYNC] 服务不健康: container_id={}, service_type={:?}, http={}, grpc={}, failures={}",
                                        project_id,
                                        container_info.service_type,
                                        health_status.http_healthy,
                                        health_status.grpc_healthy,
                                        health_status.consecutive_failures
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "⚠️ [SYNC] 检查容器状态失败: project_id={}, error={}",
                        project_id, e
                    );
                }
            }
        }

        if removed_count > 0 || health_checked_count > 0 {
            info!(
                "🔄 [SYNC] 容器状态同步完成: 检查={}, 移除={}, 健康检查={}",
                total, removed_count, health_checked_count
            );
        }

        Ok((total, removed_count))
    }

    /// 清理所有容器
    pub async fn cleanup_all_containers(&self) -> DockerResult<()> {
        info!("开始清理所有容器");

        let project_ids: Vec<String> = self.containers.keys().await;

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
        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
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

        let container_info = if let Some(info) = self.containers.get(project_id).await {
            info
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
        let main_network = self.get_main_network_name().await;
        info!("检查 RCoder 主网络状态: {}...", main_network);

        // 检查网络是否已存在
        match self.inspect_network(&main_network).await {
            Ok(_) => {
                info!("✅ RCoder 主网络已存在: {}", main_network);
                Ok(())
            }
            Err(_) => {
                warn!("⚠️ RCoder 主网络不存在: {}", main_network);
                warn!("⚠️ 这通常意味着主容器不在预期的网络中");
                warn!("⚠️ 请检查 Docker Compose 配置");
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

        debug!("使用ImageSelector选择镜像: {:?}", service_type);
        selector.select_image(service_type, project_overrides).await
    }

    /// 获取服务配置
    pub async fn get_service_config(
        &self,
        service_type: &shared_types::ServiceType,
    ) -> DockerResult<shared_types::ServiceImageConfig> {
        use crate::image_selector::ImageSelector;
        let selector = ImageSelector::new(self.config.multi_image_config.clone());

        debug!("获取服务配置: {:?}", service_type);
        selector.get_service_config(service_type).await
    }

    /// 获取容器网络信息
    ///
    /// # 返回
    /// - `Ok(HashMap)`: 网络名称到 IP 地址的映射
    /// - `Err(ConnectionError)`: 容器不存在或无法获取网络信息
    pub async fn get_container_network_info(
        &self,
        container_id: &str,
    ) -> DockerResult<HashMap<String, String>> {
        use bollard::query_parameters::InspectContainerOptions;

        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("获取容器信息失败: {}", e)))?;

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
                    Ok(())
                }
                Some(bollard::models::ContainerStateStatusEnum::EXITED) => {
                    let error_msg = state.error.as_deref().unwrap_or("未知错误");
                    error!(
                        "❌ 容器 {} 已退出 (退出码: {}): {}",
                        container_id, exit_code, error_msg
                    );
                    Err(DockerError::ContainerStartError(format!(
                        "容器启动后立即退出: {} (退出码: {}), 错误: {}",
                        container_id, exit_code, error_msg
                    )))
                }
                Some(bollard::models::ContainerStateStatusEnum::CREATED) => {
                    warn!("⚠️ 容器 {} 已创建但未启动", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "容器已创建但未启动: {}",
                        container_id
                    )))
                }
                Some(status) => {
                    let status_str = format!("{:?}", status);
                    error!("❌ 容器 {} 处于未知状态: {}", container_id, status_str);
                    Err(DockerError::ContainerStartError(format!(
                        "容器处于未知状态: {} - {}",
                        container_id, status_str
                    )))
                }
                None => {
                    error!("❌ 容器 {} 状态为空", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "容器状态为空: {}",
                        container_id
                    )))
                }
            }
        } else {
            error!("❌ 无法获取容器 {} 的状态信息", container_id);
            Err(DockerError::ContainerStartError(format!(
                "无法获取容器状态信息: {}",
                container_id
            )))
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
            t: Some(
                (timeout_seconds as i32)
                    .try_into()
                    .expect("timeout should be within valid range"),
            ),
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
                                "🧹 从内部映射中移除: project_id={}, container_id={}",
                                info.project_id, container_id
                            );
                        }
                    }
                }
            }
        }
    }

    /// 获取主网络名称（异步，返回动态检测的值）
    pub async fn get_main_network_name(&self) -> String {
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
                "无法获取 HOSTNAME 环境变量。请确保代码运行在 Docker 容器中。".to_string(),
            )
        })?;

        debug!("🔍 检测到容器 hostname: {}", hostname);

        // 直接 inspect 当前容器（hostname 通常是容器 ID 的前12位，但 Docker API 支持前缀匹配）
        let inspect = docker
            .inspect_container(&hostname, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!(
                    "无法获取当前容器信息 (hostname: {}): {}",
                    hostname, e
                ))
            })?;

        // 获取网络配置
        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks {
                // 查找包含指定网络基础名称的网络
                for network_name in networks.keys() {
                    if network_name.contains(network_base_name) {
                        info!("✅ 动态检测到主网络: {}", network_name);
                        return Ok(network_name.clone());
                    }
                }

                // 如果没找到包含指定基础名称的，返回错误
                let available_networks: Vec<String> = networks.keys().cloned().collect();
                return Err(DockerError::ConnectionError(format!(
                    "当前容器未连接到包含 '{}' 的网络。\n\
                     可用网络: {:?}\n\
                     请检查 Docker Compose 配置中的网络设置。",
                    network_base_name, available_networks
                )));
            }

        Err(DockerError::ConnectionError(format!(
            "当前容器 (hostname: {}) 没有网络配置信息",
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

    /// 测试通过容器名称获取创建时间
    ///
    /// 使用真实容器 `rcoder-rcoder-1` 验证时间戳解析
    #[tokio::test]
    #[ignore] // 需要本地环境有 Docker 和容器，默认忽略
    #[allow(deprecated)] // 测试代码使用 deprecated API 是可接受的
    async fn test_get_container_creation_time_by_name_real() {
        // 直接使用 Bollard 创建 Docker 客户端
        let docker = Docker::connect_with_local_defaults()
            .expect("Failed to connect to Docker");

        // 测试容器名称
        let container_name = "rcoder-rcoder-1";

        println!("\n🔍 测试容器: {}", container_name);
        println!("─────────────────────────────────────────");

        // 直接调用 Docker API 获取容器信息
        match docker
            .inspect_container(container_name, None::<bollard::query_parameters::InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                println!("✅ 成功获取容器信息");

                // 获取创建时间字符串
                if let Some(ref created_str) = details.created {
                    println!("   Docker API 返回: {}", created_str);

                    // 解析时间戳
                    match DateTime::parse_from_rfc3339(&created_str) {
                        Ok(created_time) => {
                            let created_time_utc = created_time.with_timezone(&Utc);
                            println!("   解析后的 UTC:  {}", created_time_utc);

                            // 计算容器年龄
                            let age = Utc::now().signed_duration_since(created_time_utc);
                            println!("   容器年龄: {} 秒", age.num_seconds());
                            println!("   容器年龄: {} 分钟", age.num_minutes());
                            println!("   容器年龄: {} 小时", age.num_hours());
                            println!("   容器年龄: {} 天", age.num_days());

                            // 验证时间是否合理
                            assert!(created_time_utc < Utc::now(), "创建时间应该在过去");
                            assert!(age.num_days() < 365, "创建时间不应该超过 1 年");

                            println!("\n✅ 时间戳解析验证通过！");
                        }
                        Err(e) => {
                            panic!("❌ RFC3339 时间戳解析失败: {}", e);
                        }
                    }
                } else {
                    panic!("❌ 容器没有 created 字段");
                }

                // 使用 Docker CLI 对比验证
                println!("\n🔍 使用 Docker CLI 对比验证:");
                println!("─────────────────────────────────────────");

                use std::process::Command;
                let output = Command::new("docker")
                    .args(&["inspect", container_name, "--format", "{{.Created}}"])
                    .output()
                    .expect("Failed to run docker inspect");

                let docker_cli_time = String::from_utf8_lossy(&output.stdout);
                println!("   Docker CLI 时间: {}", docker_cli_time.trim());

                // 解析 Docker CLI 返回的时间
                if let Ok(docker_time) = DateTime::parse_from_rfc3339(docker_cli_time.trim()) {
                    let docker_time_utc = docker_time.with_timezone(&Utc);
                    println!("   Docker CLI UTC:  {}", docker_time_utc);

                    // 从 Docker API 获取的时间
                    if let Some(ref created_str) = details.created {
                        if let Ok(api_time) = DateTime::parse_from_rfc3339(&created_str) {
                            let api_time_utc = api_time.with_timezone(&Utc);
                            println!("   API 返回 UTC:    {}", api_time_utc);

                            // 时间差应该为 0（应该完全一致）
                            let diff = (docker_time_utc.timestamp() - api_time_utc.timestamp()).abs();
                            println!("   时间差: {} 秒", diff);

                            assert_eq!(diff, 0, "API 和 CLI 返回的时间应该完全一致");
                            println!("\n✅ 与 Docker CLI 对比验证通过！");
                        }
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

        println!("\n🔍 测试 Unix 秒时间戳解析（验证 bug 修复）");
        println!("─────────────────────────────────────────");

        // 容器实际创建时间: 2026-01-19T07:35:53Z
        let expected_time = Utc.with_ymd_and_hms(2026, 1, 19, 7, 35, 53).unwrap();
        let unix_timestamp = expected_time.timestamp(); // 1768808153 秒

        println!("   预期时间: {}", expected_time);
        println!("   Unix 秒时间戳: {}", unix_timestamp);

        // 使用我们的解析函数
        match DockerManager::parse_unix_timestamp(unix_timestamp, "test") {
            Ok(parsed_time) => {
                println!("   解析结果: {}", parsed_time);

                let diff = (parsed_time.timestamp() - expected_time.timestamp()).abs();
                println!("   时间差: {} 秒", diff);

                assert_eq!(diff, 0, "时间戳解析应该完全准确");
                println!("\n✅ Unix 时间戳解析正确！");
            }
            Err(e) => {
                panic!("❌ 解析失败: {}", e);
            }
        }

        // 验证旧代码的错误
        println!("\n🔍 验证旧代码的 bug:");
        let wrong_seconds = unix_timestamp / 1000; // 旧代码的错误处理
        let wrong_time = Utc.timestamp_opt(wrong_seconds, 0).single().unwrap();
        println!("   旧代码处理: {} (错误！)", wrong_time);
        println!("   与正确时间相差: {} 天",
                 (expected_time.timestamp() - wrong_time.timestamp()) / 86400);
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
            CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
            ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
        };
        use futures_util::TryStreamExt;

        // 连接 Docker
        let docker = Docker::connect_with_local_defaults()
            .expect("Failed to connect to Docker");

        // 测试容器名称（使用时间戳避免冲突）
        let container_name = format!("test-timestamp-{}", chrono::Utc::now().timestamp());

        println!("\n🔍 测试时间戳解析");
        println!("─────────────────────────────────────────");
        println!("   容器名称: {}", container_name);

        // 拉取 alpine 镜像（如果不存在）
        println!("\n📥 拉取镜像: alpine:latest");
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

        println!("✅ 容器已创建: {}", create_result.id);

        // 2. 启动容器
        docker
            .start_container(&container_name, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .expect("Failed to start test container");

        println!("✅ 容器已启动");

        // 等待容器完全启动
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 3. 使用 list_containers API 获取 Unix 时间戳
        println!("\n📋 测试 list_containers API (Unix 时间戳):");
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
        println!("   原始 Unix 时间戳: {} 秒", unix_timestamp);

        // 使用 parse_unix_timestamp 解析
        let parsed_unix_time = DockerManager::parse_unix_timestamp(
            unix_timestamp,
            &format!("container {}", container_name),
        )
        .expect("parse_unix_timestamp 应该成功");

        println!("   解析后时间 (UTC): {}", parsed_unix_time);

        // 4. 使用 inspect_container API 获取 RFC3339 时间戳
        println!("\n📋 测试 inspect_container API (RFC3339 时间戳):");
        println!("─────────────────────────────────────────");

        let details = docker
            .inspect_container(&container_name, None::<InspectContainerOptions>)
            .await
            .expect("Failed to inspect container");

        let rfc3339_str = details.created.expect("容器应该有 created 字段");
        println!("   原始 RFC3339 字符串: {}", rfc3339_str);

        // 使用 parse_rfc3339_timestamp 解析
        let parsed_rfc3339_time = DockerManager::parse_rfc3339_timestamp(
            &rfc3339_str,
            &format!("container {}", container_name),
        )
        .expect("parse_rfc3339_timestamp 应该成功");

        println!("   解析后时间 (UTC): {}", parsed_rfc3339_time);

        // 5. 验证两个解析结果的一致性
        println!("\n🔍 验证两个 API 的时间一致性:");
        println!("─────────────────────────────────────────");

        let time_diff = (parsed_unix_time.timestamp() - parsed_rfc3339_time.timestamp()).abs();
        println!("   list_containers 时间: {}", parsed_unix_time);
        println!("   inspect_container 时间: {}", parsed_rfc3339_time);
        println!("   时间差: {} 秒", time_diff);

        // 两个 API 应该返回相同的时间（允许 1 秒误差，因为精度不同）
        assert!(
            time_diff <= 1,
            "两个 API 的时间差应该在 1 秒以内，实际差异: {} 秒",
            time_diff
        );

        // 6. 验证时间合理性
        println!("\n🔍 验证时间合理性:");
        println!("─────────────────────────────────────────");

        let now = Utc::now();
        let age = now.signed_duration_since(parsed_unix_time);

        println!("   当前时间: {}", now);
        println!("   容器年龄: {} 秒", age.num_seconds());

        assert!(age.num_seconds() >= 0, "容器创建时间应该在过去");
        assert!(age.num_seconds() < 60, "容器应该是刚创建的（< 60 秒）");

        println!("\n✅ 所有时间戳解析测试通过！");

        // 7. 清理测试容器
        println!("\n🧹 清理测试容器...");

        let remove_options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .build();

        docker
            .remove_container(&container_name, Some(remove_options))
            .await
            .expect("Failed to cleanup test container");

        println!("✅ 测试容器已清理: {}", container_name);
    }
}

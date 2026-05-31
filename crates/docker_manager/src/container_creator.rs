//! 容器创建逻辑
//!
//! 从 DockerManager::create_container() 提取，职责单一：
//! 接收 DockerContainerConfig，构建并启动 Docker 容器，返回 DockerContainerInfo。
//!
//! ## 创建流程
//! 1. 生成容器名称（使用 pod_id 或 project_id 作为标识）
//! 2. 检查已有容器（复用运行中的容器或清理已停止的容器）
//! 3. 清理内存缓存
//! 4. 确保 Docker 镜像存在（按需拉取）
//! 5. 构建挂载点、环境变量、端口映射、主机配置
//! 6. 构建网络配置（连接到主网络或使用 host 模式）
//! 7. 创建并启动容器
//! 8. 健康检查 + 等待网络就绪
//! 9. 返回容器信息

use std::collections::{HashMap, HashSet};

use bollard::models::{ContainerCreateBody, HostConfig, Mount, NetworkingConfig, PortBinding};
use bollard::query_parameters::{CreateContainerOptions, StartContainerOptions};
use chrono::Utc;
use tracing::{debug, error, info, warn};

use super::manager::DockerManager;
use super::{
    ContainerStatus, DockerContainerConfig, DockerContainerInfo, DockerError, DockerResult,
};

/// 容器创建器 — 封装 create_container 的复杂逻辑
///
/// 通过借用 DockerManager 访问 Docker 客户端、缓存和配置。
/// 主方法 `create()` 编排完整的容器创建流程。
pub(crate) struct ContainerCreator<'a> {
    manager: &'a DockerManager,
}

impl<'a> ContainerCreator<'a> {
    pub fn new(manager: &'a DockerManager) -> Self {
        Self { manager }
    }

    /// 创建并启动容器（主入口）
    ///
    /// 由于 DockerContainerConfig 的 env_vars 字段会被 into_iter() 消耗，
    /// 需要提前提取所有需要的字段。
    pub async fn create(
        &self,
        config: DockerContainerConfig,
    ) -> DockerResult<DockerContainerInfo> {
        info!(
            "[CREATE] Starting container creation: project_id={}",
            config.project_id
        );

        // 提前提取所有需要的字段（config 后面会被部分消耗）
        let project_id = config.project_id.clone();
        let image = config.image.clone();
        let host_path = config.host_path.clone();
        let container_path = config.container_path.clone();
        let port_bindings = config.port_bindings.clone();
        let auto_remove = config.auto_remove;
        let work_dir = config.work_dir.clone();
        let network_mode = config.network_mode.clone();
        let network_name_config = config.network_name.clone();
        let command = config.command.clone();
        let entrypoint = config.entrypoint.clone();
        let resource_limits = config.resource_limits.clone();
        let extra_mounts = config.extra_mounts.clone();

        // 1. 生成容器名称
        let container_identifier = config.pod_id.as_ref().unwrap_or(&project_id);
        let container_name = super::utils::DockerUtils::generate_container_name(
            &config.name_prefix,
            container_identifier,
        )
        .map_err(DockerError::ConfigurationError)?;

        // 2. 检查并复用已有容器
        if let Some(info) = self
            .try_reuse_existing_container(&container_name, &project_id, &image)
            .await?
        {
            return Ok(info);
        }

        // 3. 清理内存缓存
        self.cleanup_cache(container_identifier, &project_id).await;

        // 4. 确保镜像存在
        self.manager.ensure_image_exists(&image).await?;

        // 5. 构建挂载点
        let mounts = build_mounts(&host_path, &container_path, &extra_mounts);

        // 6. 构建环境变量（消耗 config.env_vars）
        let env_vars = build_env_vars(config.env_vars);

        // 7. 构建端口映射
        let port_bindings_map = build_port_bindings(&port_bindings);

        // 8. 构建主机配置
        let host_config = build_host_config(
            mounts,
            port_bindings_map,
            auto_remove,
            resource_limits.as_ref(),
        );

        // 9. 构建网络配置和容器体
        let (networking_config, container_network_name) = self
            .build_networking(&container_name, &network_mode, network_name_config.as_ref())
            .await;

        let mut container_body = ContainerCreateBody {
            image: Some(image.clone()),
            working_dir: Some(work_dir),
            env: Some(env_vars),
            host_config: Some(host_config),
            networking_config,
            tty: Some(true),
            open_stdin: Some(true),
            hostname: Some(format!(
                "agent-{}",
                project_id.chars().take(8).collect::<String>()
            )),
            domainname: Some("rcoder.local".to_string()),
            ..Default::default()
        };

        if let Some(cmd) = command {
            container_body.cmd = Some(cmd);
        }
        if let Some(entry) = entrypoint {
            container_body.entrypoint = Some(entry);
        }

        // 10. 创建并启动容器
        let container_id = self
            .docker_create_and_start(&container_name, container_body)
            .await?;

        // 11. 等待并健康检查
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Err(e) = self.manager.check_container_health(&container_id).await {
            // 健康检查失败：容器已启动但不健康，回滚停止容器防止孤儿泄漏
            warn!(
                "[CREATE] Container {} started but health check failed: {}. Rolling back...",
                container_id, e
            );
            let _ = self.manager.stop_container_by_id(&container_id).await;
            return Err(e);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // 12. 构建并返回容器信息
        let container_info = DockerContainerInfo {
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            project_id: project_id.clone(),
            user_id: None,
            service_type: None,
            image,
            status: ContainerStatus::Running,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            host_path,
            container_path,
            port_bindings,
            assigned_port: 0,
            health_status: None,
            service_health: None,
            internal_port: 8080,
            network_name: container_network_name.clone(),
        };

        self.manager
            .containers
            .insert(project_id, container_info.clone())
            .await;

        info!(
            "[CREATE] Container created and started: {} (ID: {}) - network {}",
            container_name, container_id, container_network_name
        );

        Ok(container_info)
    }

    /// 尝试复用已有的运行中容器
    ///
    /// 如果同名容器正在运行且 IP 有效，直接复用。
    /// 如果容器已停止或 IP 为空，删除旧容器后返回 None。
    async fn try_reuse_existing_container(
        &self,
        container_name: &str,
        project_id: &str,
        image: &str,
    ) -> DockerResult<Option<DockerContainerInfo>> {
        let result = match self.manager.find_container_realtime(container_name).await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(_) => return Ok(None),
        };

        if result.is_running && !result.container_ip.is_empty() {
            info!(
                "[CREATE] Container already running, reusing: name={}, id={}, ip={}",
                result.container_name, result.container_id, result.container_ip
            );

            let info = DockerContainerInfo::new(
                result.container_id,
                result.container_name,
                project_id.to_string(),
                image.to_string(),
            );

            self.manager
                .containers
                .insert(
                    project_id.to_string(),
                    info.clone(),
                )
                .await;

            return Ok(Some(info));
        }

        // 容器已停止或 IP 为空，清理
        if result.is_running {
            warn!(
                "[CREATE] Container running but empty IP, deleting: name={}",
                result.container_name
            );
        } else {
            warn!(
                "[CREATE] Found stopped container, deleting: name={}, status={:?}",
                result.container_name, result.status
            );
        }

        if let Err(e) = self.manager.stop_container_by_id(&result.container_id).await {
            error!("[CREATE] Failed to delete old container: {}", e);
        }

        Ok(None)
    }

    /// 清理内存缓存中的旧容器记录
    async fn cleanup_cache(&self, _container_identifier: &str, project_id: &str) {
        if let Some(existing) = self.manager.containers.get(project_id).await {
            warn!(
                "[CREATE] Cleaning stale cache record: project_id={}, container_name={}",
                project_id, existing.container_name
            );
            self.manager.containers.remove(project_id).await;
        }
    }

    /// 构建网络配置
    ///
    /// 非 host 模式：连接到 Docker 主网络
    /// host 模式：不使用网络配置
    async fn build_networking(
        &self,
        container_name: &str,
        network_mode: &str,
        network_name_override: Option<&String>,
    ) -> (Option<NetworkingConfig>, String) {
        if network_mode != "host" {
            let main_network = self.manager.get_main_network_name().await;
            let network_name = network_name_override.unwrap_or(&main_network);

            let mut endpoints = HashMap::new();
            endpoints.insert(
                network_name.clone(),
                bollard::models::EndpointSettings {
                    aliases: Some(vec![container_name.to_string()]),
                    ..Default::default()
                },
            );

            info!(
                "[NETWORK] Container {} connecting to: {}",
                container_name, network_name
            );

            (
                Some(NetworkingConfig {
                    endpoints_config: Some(endpoints),
                }),
                network_name.clone(),
            )
        } else {
            info!("[NETWORK] Container {} uses host network", container_name);
            (None, "host".to_string())
        }
    }

    /// 通过 Docker API 创建并启动容器
    async fn docker_create_and_start(
        &self,
        container_name: &str,
        body: ContainerCreateBody,
    ) -> DockerResult<String> {
        let create_options = CreateContainerOptions {
            name: Some(container_name.to_string()),
            platform: self.manager.config.default_platform.clone(),
        };

        let result = self
            .manager
            .docker
            .create_container(Some(create_options), body)
            .await
            .map_err(|e| {
                DockerError::ContainerCreationError(format!("failed to create container: {}", e))
            })?;

        let container_id = result.id.clone();

        self.manager
            .docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ContainerStartError(format!("failed to start container: {}", e))
            })?;

        Ok(container_id)
    }
}

// ============================================================
// 独立构建函数（纯函数，不依赖 DockerManager）
// ============================================================

/// 构建挂载点列表
///
/// 按 container_path 去重，防止 Docker "Duplicate mount point" 错误
fn build_mounts(
    host_path: &str,
    container_path: &str,
    extra_mounts: &[crate::types::MountPoint],
) -> Vec<Mount> {
    let mut mounts = Vec::new();
    let mut mounted_targets: HashSet<String> = HashSet::new();

    // 主挂载点（仅在 host_path 非空时添加）
    if !host_path.is_empty() {
        mounted_targets.insert(container_path.to_string());
        mounts.push(Mount {
            target: Some(container_path.to_string()),
            source: Some(host_path.to_string()),
            typ: Some(bollard::models::MountType::BIND),
            read_only: Some(false),
            ..Default::default()
        });
        debug!("[MOUNT] Primary: {} -> {}", host_path, container_path);
    } else {
        debug!("[MOUNT] Skip primary mount, using extra_mounts only");
    }

    // 额外挂载点
    for extra in extra_mounts {
        if !mounted_targets.insert(extra.container_path.clone()) {
            warn!(
                "[MOUNT] Skipping duplicate target: {} (host: {})",
                extra.container_path, extra.host_path
            );
            continue;
        }
        mounts.push(Mount {
            target: Some(extra.container_path.clone()),
            source: Some(extra.host_path.clone()),
            typ: Some(bollard::models::MountType::BIND),
            read_only: Some(extra.read_only),
            ..Default::default()
        });
    }

    mounts
}

/// 构建环境变量列表
///
/// 将 HashMap 转换为 Docker 的 "KEY=VALUE" 格式
fn build_env_vars(env_vars: HashMap<String, String>) -> Vec<String> {
    #[allow(unused_mut)]
    let mut vars: Vec<String> = env_vars
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    // eBPF 调试模式：启用自动火焰图和持续剖析
    #[cfg(feature = "ebpf-debug")]
    {
        vars.push("SAMPLE_DURATION=30".to_string());
        vars.push("GENERATE_INTERVAL=60".to_string());
        vars.push("MAX_FLAMEFILES=50".to_string());
        vars.push("MAX_OFFCPU_FILES=50".to_string());
        vars.push("ENABLE_EBPF_AUTO_FLAMEGRAPH=true".to_string());
        vars.push("ENABLE_ALLOY=true".to_string());
        vars.push("PYROSCOPE_URL=http://pyroscope:4040".to_string());
        vars.push("ENABLE_OFFCPUTIME=true".to_string());
        vars.push("OFFCPU_DURATION=30".to_string());
        vars.push("OFFCPU_INTERVAL=60".to_string());
        vars.push("ENABLE_SYSCALL_MONITOR=true".to_string());
    }

    vars
}

/// 构建端口映射
fn build_port_bindings(
    port_bindings: &HashMap<String, String>,
) -> HashMap<String, Option<Vec<PortBinding>>> {
    let mut map = HashMap::new();
    for (container_port, host_port) in port_bindings {
        map.insert(
            container_port.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port.clone()),
            }]),
        );
    }
    map
}

/// 构建主机配置
fn build_host_config(
    mounts: Vec<Mount>,
    port_bindings: HashMap<String, Option<Vec<PortBinding>>>,
    auto_remove: bool,
    resource_limits: Option<&crate::types::ResourceLimits>,
) -> HostConfig {
    let mut config = HostConfig {
        mounts: Some(mounts),
        port_bindings: Some(port_bindings),
        auto_remove: Some(auto_remove),
        #[cfg(feature = "ebpf-debug")]
        privileged: Some(true),
        #[cfg(not(feature = "ebpf-debug"))]
        privileged: Some(false),
        #[cfg(feature = "ebpf-debug")]
        cap_add: Some(vec![
            "SYS_ADMIN".to_string(),
            "NET_ADMIN".to_string(),
            "SYS_PTRACE".to_string(),
        ]),
        #[cfg(not(feature = "ebpf-debug"))]
        cap_drop: Some(vec!["NET_RAW".to_string(), "NET_ADMIN".to_string()]),
        ..Default::default()
    };

    if let Some(limits) = resource_limits {
        config.memory = limits.memory_limit.and_then(|v| {
            if v.is_finite() && v >= 0.0 && v <= i64::MAX as f64 {
                Some(v as i64)
            } else {
                warn!("[DOCKER_MGR] memory_limit {} out of range, skipping", v);
                None
            }
        });
        config.memory_swap = limits.swap_limit.and_then(|v| {
            if v.is_finite() && v >= 0.0 && v <= i64::MAX as f64 {
                Some(v as i64)
            } else {
                warn!("[DOCKER_MGR] swap_limit {} out of range, skipping", v);
                None
            }
        });
        if let Some(cpu_limit) = limits.cpu_limit {
            let nano_cpus = cpu_limit * 1e9;
            if nano_cpus.is_finite() && nano_cpus >= 0.0 && nano_cpus <= i64::MAX as f64 {
                config.nano_cpus = Some(nano_cpus as i64);
            } else {
                warn!("[DOCKER_MGR] cpu_limit {} results in nano_cpus {} out of range, skipping",
                      cpu_limit, nano_cpus);
            }
        }
    }

    config
}

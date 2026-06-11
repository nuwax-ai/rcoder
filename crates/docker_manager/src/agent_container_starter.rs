//! Agent 容器启动编排
//!
//! 从 DockerManager::start_agent_container() 提取。
//! 职责：参数解析 → 旧容器清理 → 配置准备 → 委托 create_container → 健康检查

use container_runtime_api::ContainerCreateParams;
use chrono::Utc;
use shared_types::{ContainerBasicInfo, IsolationType, ServiceType};
use std::str::FromStr;
use tracing::{debug, info, warn};

use super::manager::DockerManager;
use super::{DockerError, DockerResult};

/// Agent 容器启动器
///
/// 编排完整的 Agent 容器启动流程：
/// 1. 预检查工作目录
/// 2. 清理旧容器
/// 3. 获取服务配置和镜像
/// 4. 构建容器配置（挂载、环境变量、网络）
/// 5. 委托 create_container 创建并启动
/// 6. 等待健康检查通过
pub(crate) struct AgentContainerStarter<'a> {
    manager: &'a DockerManager,
}

impl<'a> AgentContainerStarter<'a> {
    pub fn new(manager: &'a DockerManager) -> Self {
        Self { manager }
    }



    pub async fn start(
        &self,
        params: ContainerCreateParams,
    ) -> DockerResult<ContainerBasicInfo> {
        let ContainerCreateParams {
            project_id,
            user_id,
            host_workspace_path,
            service_type,
            resource_limits: request_resource_limits,
            pod_id,
            isolation_type,
            tenant_id,
            space_id,
        } = params;

        info!(
            "Starting Agent container: project_id={:?}, user_id={:?}, type={:?}, host_path={}, pod_id={:?}, isolation_type={:?}",
            project_id, user_id, service_type, host_workspace_path, pod_id, isolation_type
        );

        // 1. 在宿主机上预创建工作目录
        // 1. 检查工作目录是否已存在（通过绑定挂载，容器内创建会自动同步）
        debug!("[DOCKER_MGR] checkworkdirectory: {}", host_workspace_path);
        // 绑定挂载机制：容器内创建目录会自动同步到宿主机
        // 所以这里不需要额外创建目录

        // 2. 清理旧容器（如果提供了 project_id）
        if let Some(ref id) = project_id
            && let Some(existing) = self.manager.get_container_info(id).await
        {
            warn!(
                "Stopping container {}, already stopped...",
                existing.container_name
            );
            self.manager.stop_container(id).await?;
        }

        // 2. 获取配置和镜像
        let service_config = self.manager.get_service_config(&service_type).await?;
        let image = self.manager.select_image(&service_type, None).await?;

        // 3. 准备配置
        use crate::container_builder::ContainerConfigBuilder;

        // 确定用于构建容器配置的主 ID
        // 标准 RCoder 使用 project_id，Computer Agent Runner 使用 user_id
        let container_id: String = if let Some(ref uid) = user_id {
            // Computer Agent Runner 使用 user_id
            uid.clone()
        } else if let Some(ref pid) = project_id {
            // 标准 RCoder 使用 project_id
            pid.clone()
        } else {
            // 错误：至少需要提供 project_id 或 user_id 其中一个
            return Err(DockerError::ConfigurationError(
                "Must provide at least one of project_id or user_id".to_string(),
            ));
        };

        // 解析容器内工作目录路径
        let mut variables = std::collections::HashMap::new();
        // 根据服务类型设置相应的变量
        if let Some(ref pid) = project_id {
            variables.insert("project_id".to_string(), pid.clone());
        }
        if let Some(ref uid) = user_id {
            variables.insert("user_id".to_string(), uid.clone());
        }
        variables.insert("service_type".to_string(), service_type.to_string());

        // 添加隔离类型相关变量（用于挂载路径解析）
        if let Some(ref pid) = pod_id {
            variables.insert("pod_id".to_string(), pid.clone());
        }
        if let Some(ref it) = isolation_type {
            variables.insert("isolation_type".to_string(), it.clone());
        }
        if let Some(ref tid) = tenant_id {
            variables.insert("tenant_id".to_string(), tid.clone());
        }
        if let Some(ref sid) = space_id {
            variables.insert("space_id".to_string(), sid.clone());
        }

        let container_work_path = service_config.resolve_container_path(&variables);

        // 构建基础配置
        let mut builder = ContainerConfigBuilder::new(container_id.clone())
            .image(image)
            .name_prefix(service_config.container_prefix())
            .work_dir(service_config.work_dir.clone())
            .network_mode(service_config.network_mode.clone())
            .auto_remove(true);

        // 添加隔离类型相关配置
        if let Some(ref pid) = pod_id {
            builder = builder.pod_id(pid.clone());
        }
        if let Some(ref it) = isolation_type {
            builder = builder.isolation_type(it.clone());
        }
        // 保存引用供后续使用
        let tenant_id_ref = tenant_id.as_deref();
        let space_id_ref = space_id.as_deref();
        if let Some(tid) = tenant_id_ref {
            builder = builder.tenant_id(tid);
        }
        if let Some(sid) = space_id_ref {
            builder = builder.space_id(sid);
        }

        // 只在 host_workspace_path 非空时添加主挂载点
        // 如果为空，表示完全依赖 mounts 配置（例如 ComputerAgentRunner）
        if !host_workspace_path.is_empty() {
            builder = builder
                .host_path(host_workspace_path.to_string())
                .container_path(container_work_path.clone());

            debug!(
                "📌 [DOCKER_MANAGER] Main mount: {} -> {}",
                host_workspace_path, container_work_path
            );
        } else {
            debug!("📌 [DOCKER_MANAGER] Skip mount, no mounts config");
        }

        // 先获取借用字段，因为后续字段会被移动
        let container_prefix = service_config.container_prefix().to_string();
        let workspace_resolution = service_config.effective_workspace_resolution_path();
        let workspace_container = service_config.workspace_container_path();

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
        // 处理其他环境变量中的模板（先处理，因为后续需要使用 project_id/user_id 的值）
        for (key, value) in &service_config.environment {
            let mut processed_value = value.clone();
            if let Some(ref pid) = project_id {
                processed_value = processed_value.replace("{project_id}", pid);
            }
            if let Some(ref uid) = user_id {
                processed_value = processed_value.replace("{user_id}", uid);
            }
            builder = builder.env(key, &processed_value);
        }

        // 根据服务类型设置相应的环境变量（最后设置，覆盖模板处理的值）
        if let Some(ref pid) = project_id {
            builder = builder.env("PROJECT_ID", pid);
        }
        if let Some(ref uid) = user_id {
            builder = builder.env("USER_ID", uid);
        }
        // 隔离模式相关环境变量（agent_runner 用于构建工作目录路径）
        if let Some(ref tid) = tenant_id {
            builder = builder.env("TENANT_ID", tid);
        }
        if let Some(ref sid) = space_id {
            builder = builder.env("SPACE_ID", sid);
        }
        if let Some(ref it) = isolation_type {
            builder = builder.env("ISOLATION_TYPE", it);
        }

        // 注意：子容器以 root 用户运行，不再需要 UID/GID 匹配

        // 设置网络
        let network_name = self.manager.get_main_network_name().await;
        builder = builder.network_name(network_name);

        // ===== 自动注入 workspace 挂载 =====
        // workspace_resolution: rcoder 容器内路径 → 解析宿主机路径
        // workspace_container: sub-container 内挂载目标
        //
        // auto-inject 统一处理 workspace 隔离挂载，config mounts 中的 workspace 挂载
        // 通过 resolve_from 精确匹配跳过，避免重复。

        // 记录 auto-inject 已添加的 container_path，用于后续冲突检测
        let mut auto_injected_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        match crate::path::resolve_container_path_to_host(std::path::Path::new(
            &workspace_resolution,
        ))
        .await
        {
            Ok(workspace_host_path) => {
                let (host_sub, container_mount) = if pod_id.is_some() {
                    // pod_id 有值：根据 isolation_type 决定挂载级别
                    // 必须校验必填字段，遵循 Fail Fast 原则
                    let isolation = match isolation_type {
                        Some(ref s) => IsolationType::from_str(s).map_err(|e| {
                            DockerError::ContainerCreationError(e.to_string())
                        })?,
                        None => IsolationType::default(),
                    };

                    match isolation {
                        IsolationType::Space => {
                            // space 隔离需要 tenant_id 和 space_id
                            let tid = tenant_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "tenant_id is required when isolation_type is 'space' and pod_id is provided".to_string()
                                )
                            })?;
                            let sid = space_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "space_id is required when isolation_type is 'space' and pod_id is provided".to_string()
                                )
                            })?;
                            let sub = format!("{}/{}", tid, sid);
                            (
                                sub.clone(),
                                std::path::PathBuf::from(&workspace_container).join(&sub),
                            )
                        }
                        IsolationType::Tenant => {
                            // tenant 隔离只需要 tenant_id
                            let tid = tenant_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "tenant_id is required when isolation_type is 'tenant' and pod_id is provided".to_string()
                                )
                            })?;
                            let sub = tid.to_string();
                            (
                                sub.clone(),
                                std::path::PathBuf::from(&workspace_container).join(&sub),
                            )
                        }
                        IsolationType::Project => {
                            // project 隔离需要 tenant_id、space_id 和 project_id
                            let tid = tenant_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "tenant_id is required when pod_id is provided".to_string()
                                )
                            })?;
                            let sid = space_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "space_id is required when pod_id is provided".to_string()
                                )
                            })?;
                            let proj = project_id.as_deref().ok_or_else(|| {
                                DockerError::ContainerCreationError(
                                    "project_id is required when pod_id is provided".to_string()
                                )
                            })?;
                            let sub = format!("{}/{}/{}", tid, sid, proj);
                            (
                                sub.clone(),
                                std::path::PathBuf::from(&workspace_container).join(&sub),
                            )
                        }
                    }
                } else {
                    // pod_id 无值：根据 service_type 选择挂载策略
                    match service_type {
                        // ComputerAgentRunner: 一个 user_id 对应一个容器
                        // 挂载: 宿主机 /computer-project-workspace/{user_id} → 容器 /home/user
                        // config.yml 中 container_path: "/home/user"
                        ServiceType::ComputerAgentRunner => {
                            let uid = user_id.as_deref().unwrap_or("default");
                            (
                                uid.to_string(),
                                std::path::PathBuf::from(&workspace_container),
                            )
                        }
                        // RCoder: 一个 project_id 对应一个容器
                        // 挂载: 宿主机 /project_workspace/{project_id} → 容器 /project_workspace/{project_id}
                        ServiceType::RCoder => {
                            let pid = project_id.as_deref().unwrap_or("default");
                            (
                                pid.to_string(),
                                std::path::PathBuf::from(&workspace_container).join(pid),
                            )
                        }
                    }
                };

                let host_mount = workspace_host_path.join(&host_sub);

                // 创建宿主机挂载目录（通过容器内路径创建，volume 会传播到宿主机）
                // workspace_resolution 是容器内路径（如 /app/computer-project-workspace）
                // host_sub 是子目录（如 tenant_abc）
                // 拼接后在容器内创建目录，通过 docker-compose volume 自动同步到宿主机
                let host_dir_to_create = std::path::PathBuf::from(&workspace_resolution).join(&host_sub);
                if let Err(e) = std::fs::create_dir_all(&host_dir_to_create) {
                    warn!(
                        "[DOCKER_MGR] Failed to create workspace directory {}: {}",
                        host_dir_to_create.display(),
                        e
                    );
                } else {
                    info!(
                        "[DOCKER_MGR] Created workspace directory: {}",
                        host_dir_to_create.display()
                    );
                }

                let host_mount_str = host_mount.to_string_lossy().to_string();
                let container_mount_str = container_mount.to_string_lossy().to_string();
                auto_injected_paths.insert(container_mount_str.clone());
                builder = builder.add_mount(crate::MountPoint {
                    host_path: host_mount_str,
                    container_path: container_mount_str,
                    read_only: false,
                });

                info!(
                    "📌 [DOCKER_MGR] Auto workspace mount: {} -> {}",
                    host_mount.display(),
                    container_mount.display()
                );
            }
            Err(e) => {
                warn!(
                    "⚠️ [DOCKER_MGR] Failed to resolve workspace host path, skipping auto mount: {}",
                    e
                );
            }
        }

        // 🎯 处理配置文件中的挂载点 (service_config.mounts)
        let container_name = format!("{}-{}", container_prefix, container_id);
        let timestamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
        let log_dir_name = format!("{}-{}", container_name, timestamp);

        // 设置命令（放在获取 container_name 之后，因为会移动 service_config.command）
        builder = builder.command(service_config.command);
        if let Some(entry) = service_config.entrypoint {
            builder = builder.entrypoint(entry);
        }

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
            // 跳过 workspace 挂载：auto-inject 统一处理隔离挂载
            // 精确匹配 resolve_from，不靠路径模式猜测
            if mount_config.resolve_from.as_deref() == Some(&workspace_resolution) {
                debug!(
                    "Skipping workspace mount from config (auto-inject handles isolation): \
                     container_path={}, resolve_from={}",
                    mount_config.container_path,
                    mount_config.resolve_from.as_deref().unwrap_or("")
                );
                continue;
            }

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
                                    "[DOCKER_MGR] Resolved from {} to host path: {}",
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
                                    "[DOCKER_MGR] Unable to resolve path (resolve_from: {}): {}",
                                    resolve_from_path, e
                                );
                                None
                            }
                        }
                    };

                // 添加解析后的基础路径变量
                if let Some(resolved_path) = resolved_base {
                    let normalized = resolved_path.components().collect::<std::path::PathBuf>();
                    mount_variables.insert(
                        "resolved_path".to_string(),
                        normalized.to_string_lossy().to_string(),
                    );
                } else {
                    // 如果解析失败，跳过此挂载点
                    warn!(
                        "[DOCKER_MGR] Skipping mount point (unable to resolve resolve_from): {}",
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
                    "[DOCKER_MGR] Skipping mount point (host_path contains unresolved variables): {}",
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
                "Adding mount point: {} -> {} (read_only: {})",
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
                    // 非 pod 模式：从 host_path 模板中提取相对路径部分
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
                        "Creating directory using container path: {} (resolve_from: {}, relative: {})",
                        create_path, resolve_from_path, relative_part
                    );

                    if let Err(e) = std::fs::create_dir_all(&create_path) {
                        warn!(
                            "[DOCKER_MGR] createdmountdirectoryfailed: {} - {}",
                            create_path, e
                        );
                    } else {
                        info!("[DOCKER_MGR] directorycreatedsucceeded: {}", create_path);
                    }
                } else {
                    // 没有配置 resolve_from，无法在容器内创建宿主机路径
                    // 假设目录已存在，跳过创建
                    info!(
                        "Skipping directory creation (resolve_from not configured): {}",
                        resolved_host_path
                    );
                }
            }

            // 冲突检测：如果 config 挂载的 container_path 与 auto-inject 重复，丢弃 config 挂载
            if auto_injected_paths.contains(&resolved_container_path) {
                warn!(
                    "[DOCKER_MGR] Skipping config mount (conflicts with auto-inject): \
                     {} -> {}",
                    resolved_container_path, resolved_host_path
                );
                continue;
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

        self.manager.create_container(config).await?;

        // 🆕 更新容器映射中的 user_id 和 service_type
        if let Some(mut info) = self.manager.containers.get(&container_id).await {
            info.user_id = user_id.map(|s| s.to_string());
            info.service_type = Some(service_type.clone());
            debug!(
                "📝 [DOCKER_MGR] Updating container metadata: container_id={}, user_id={:?}, service_type={:?}",
                container_id, info.user_id, info.service_type
            );
            self.manager.containers
                .insert(container_id.to_string(), info.clone())
                .await;

            // 当 pod_id 存在时，也用 pod_id 作为 key 缓存，确保后续请求通过 pod_id 能找到容器
            if let Some(ref pid) = pod_id {
                self.manager.containers.insert(pid.to_string(), info).await;
                debug!(
                    "📝 [DOCKER_MGR] Cached container under pod_id key: pod_id={}",
                    pid
                );
            }
        }

        // 5. 等待就绪并返回信息
        // 优先使用 pod_id 查找（复用场景），否则使用 container_id (project_id)
        let lookup_key = pod_id.as_deref().unwrap_or(&container_id);
        let info = self.manager.get_agent_info(lookup_key).await?.ok_or_else(|| {
            DockerError::ContainerStartError(
                "unable to get info after container started".to_string(),
            )
        })?;

        // 健康检查 - 如果失败则回滚容器
        match crate::health::wait_for_service_ready(&info.service_url).await {
            Ok(_) => {
                info!("Agent container started: {}", info.service_url);
                Ok(info)
            }
            Err(e) => {
                // 健康检查失败，回滚：停止并删除孤儿容器
                warn!(
                    "[DOCKER_MGR] Health check failed for container {}: {}. Rolling back...",
                    container_id, e
                );

                // 尝试清理容器（忽略清理过程中的错误）
                if let Err(cleanup_err) = self.manager.stop_container(&container_id).await {
                    warn!(
                        "[DOCKER_MGR] Failed to cleanup container {} after health check failure: {}",
                        container_id, cleanup_err
                    );
                } else {
                    info!(
                        "[DOCKER_MGR] Successfully rolled back container {} after health check failure",
                        container_id
                    );
                }

                // 清理 pod_id 缓存条目（stop_container 只清理 container_id 对应的 key）
                if let Some(ref pid) = pod_id {
                    self.manager.containers.remove(pid).await;
                    debug!(
                        "[DOCKER_MGR] Cleaned up pod_id cache entry: pod_id={}",
                        pid
                    );
                }

                // 返回原始健康检查错误
                Err(DockerError::ContainerStartError(format!(
                    "health check failed: {}",
                    e
                )))
            }
        }
    }

}

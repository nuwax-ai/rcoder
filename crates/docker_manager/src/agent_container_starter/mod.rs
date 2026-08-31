//! Agent 容器启动编排
//!
//! 从 DockerManager::start_agent_container() 提取。
//! 职责：参数解析 → 旧容器清理 → 配置准备 → 委托 create_container → 健康检查

use container_runtime_api::ContainerCreateParams;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::time::Instant;
use tracing::{debug, info, warn};

use super::manager::DockerManager;

mod mounts;
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

    pub async fn start(&self, params: ContainerCreateParams) -> DockerResult<ContainerBasicInfo> {
        let ContainerCreateParams {
            project_id,
            user_id,
            service_type,
            resource_limits: request_resource_limits,
            pod_id,
            isolation_type,
            tenant_id,
            space_id,
            // Docker 模式忽略 storage_size（仅 K8s 模式使用）；Userapp 专用字段
            // （image_override/command/env/ports/...）由 DockerRuntime::create_deployment 处理，
            // agent 路径一并忽略。
            ..
        } = params;

        let start_phase = Instant::now();

        info!(
            "Starting Agent container: project_id={:?}, user_id={:?}, type={:?}, pod_id={:?}, isolation_type={:?}",
            project_id, user_id, service_type, pod_id, isolation_type
        );

        // 挂载目录预创建由 apply_auto_mounts 统一处理（绑定挂载机制：rcoder 容器内
        // 创建目录会自动同步宿主机，bind 源即刻可见）。

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

        // 确定用于构建容器配置的主 ID（复用 ServiceType::container_identifier 单一事实源）
        let container_id: String = service_type
            .container_identifier(pod_id.as_deref(), user_id.as_deref(), project_id.as_deref())
            .map_err(|e| DockerError::ConfigurationError(e.to_string()))?
            .to_string();

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

        // 构建基础配置（workspace 挂载统一走 apply_auto_mounts 的 auto-inject，
        // host_workspace_path 参数已退役——历史恒空串，主挂载分支从不触发）。
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

        // 先获取借用字段，因为后续字段会被移动
        let container_prefix = service_config.container_prefix().to_string();
        let workspace_resolution = service_config.effective_workspace_resolution_path();
        let workspace_container = service_config.workspace_container_path();

        // 应用资源限制
        let limits = service_config.resource_limits.clone();

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

        // final_resource_limits 已是 ServiceResourceLimits，直接传给 builder
        // （Docker 只用 memory/cpu/swap，storage 字段随 struct 携带但 build_host_config 不读）
        builder = builder.resource_limits(final_resource_limits);

        // 透传服务级安全配置（仅 Docker 模式生效；None 时 build_host_config 走代码默认）
        builder = builder.security(service_config.security.clone());

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

        // 部署模式标识: start-up.sh 据此 source extra (Docker Compose 下 /home/user 是 bind mount, 需修权限)
        builder = builder.env("DEPLOY_MODE", "docker");

        // UserappBuilder 挂载压平契约 env（与 mounts.rs 三 bind 挂载点绑定, 值为
        // shared_types::paths 单一事实源; 最后设置覆盖 config environment——否则
        // PGDATA 落 overlay, builder 重建丢库）。PGDATA/DBX_DATA_DIR 使 dev 数据
        // 落卷持久（镜像 start-up.sh 均为 ${VAR:-...} 覆盖模式）。
        if matches!(service_type, ServiceType::UserappBuilder) {
            builder = builder
                .env(
                    "USERAPP_WORKSPACE_DIR",
                    shared_types::paths::USERAPP_DEV_HOME,
                )
                .env("USERAPP_LOG_DIR", shared_types::paths::USERAPP_DEV_LOGS)
                .env("PGDATA", shared_types::paths::USERAPP_DEV_PGDATA)
                .env("DBX_DATA_DIR", shared_types::paths::USERAPP_DEV_DBX_DATA);
        }

        // 注意：子容器以 root 用户运行，不再需要 UID/GID 匹配

        // 设置网络
        let network_name = self.manager.get_main_network_name().await;
        builder = builder.network_name(network_name);

        let builder = mounts::apply_auto_mounts(
            builder,
            &mounts::MountContext {
                container_id: &container_id,
                container_prefix: &container_prefix,
                service_config: &service_config,
                variables: &variables,
                workspace_resolution: &workspace_resolution,
                workspace_container: &workspace_container,
                service_type: &service_type,
                isolation_type: isolation_type.as_deref(),
                project_id: project_id.as_deref(),
                user_id: user_id.as_deref(),
                pod_id: pod_id.as_deref(),
                tenant_id: tenant_id.as_deref(),
                space_id: space_id.as_deref(),
            },
        )
        .await?;

        // 4. 创建并启动
        let config = builder
            .build()
            .map_err(|e| DockerError::ContainerCreationError(e.to_string()))?;

        let config_build_elapsed = start_phase.elapsed();
        info!(
            "[DOCKER_MGR] create_container starting: container_id={}, service_type={:?}, image={}, config_build_elapsed={:?}",
            container_id, service_type, config.image, config_build_elapsed
        );

        let docker_create_started = Instant::now();
        self.manager.create_container(config).await?;
        info!(
            "[DOCKER_MGR] Docker create_container finished in {:?} (total {:?}): container_id={}",
            docker_create_started.elapsed(),
            start_phase.elapsed(),
            container_id
        );

        // 🆕 更新容器映射中的 user_id 和 service_type
        if let Some(mut info) = self.manager.containers.get(&container_id).await {
            info.user_id = user_id.map(|s| s.to_string());
            info.service_type = Some(service_type.clone());
            debug!(
                "[DOCKER_MGR] Updating container metadata: container_id={}, user_id={:?}, service_type={:?}",
                container_id, info.user_id, info.service_type
            );
            self.manager
                .containers
                .insert(container_id.to_string(), info.clone())
                .await;

            // 当 pod_id 存在时，也用 pod_id 作为 key 缓存，确保后续请求通过 pod_id 能找到容器
            if let Some(ref pid) = pod_id {
                self.manager.containers.insert(pid.to_string(), info).await;
                debug!(
                    "[DOCKER_MGR] Cached container under pod_id key: pod_id={}",
                    pid
                );
            }
        }

        // 5. 等待就绪并返回信息
        // 优先使用 pod_id 查找（复用场景），否则使用 container_id (project_id)
        let lookup_key = pod_id.as_deref().unwrap_or(&container_id);
        let info = self
            .manager
            .get_agent_info(lookup_key)
            .await?
            .ok_or_else(|| {
                DockerError::ContainerStartError(
                    "unable to get info after container started".to_string(),
                )
            })?;

        // 健康检查 - 如果失败则回滚容器
        info!(
            "[DOCKER_MGR] Health check starting: container_id={}, service_url={}, elapsed_since_start={:?}",
            container_id,
            info.service_url,
            start_phase.elapsed()
        );
        let health_started = Instant::now();
        match crate::health::wait_for_service_ready(&info.service_url).await {
            Ok(_) => {
                info!(
                    "Agent container started: {} (health {:?}, total {:?})",
                    info.service_url,
                    health_started.elapsed(),
                    start_phase.elapsed()
                );
                Ok(info)
            }
            Err(e) => {
                // 健康检查失败，回滚：停止并删除孤儿容器
                warn!(
                    "[DOCKER_MGR] Health check failed for container {} after {:?} (total {:?}): {}. Rolling back...",
                    container_id,
                    health_started.elapsed(),
                    start_phase.elapsed(),
                    e
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
                    debug!("[DOCKER_MGR] Cleaned up pod_id cache entry: pod_id={}", pid);
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

//! Agent 容器启动编排
//!
//! 从 DockerManager::start_agent_container() 提取。
//! 职责：参数和配置预检 → 旧容器清理 → 委托 create_container → 健康检查

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
/// 1. 预检查身份、资源参数、服务配置和镜像
/// 2. 清理旧容器
/// 3. 使用预检结果准备配置
/// 4. 构建容器配置（挂载、环境变量、网络）
/// 5. 委托 create_container 创建并启动
/// 6. 等待健康检查通过
pub(crate) struct AgentContainerStarter<'a> {
    manager: &'a DockerManager,
}

pub(crate) struct PreparedAgentConfig {
    service_config: shared_types::ServiceImageConfig,
    image: String,
    container_id: String,
}

impl<'a> AgentContainerStarter<'a> {
    pub fn new(manager: &'a DockerManager) -> Self {
        Self { manager }
    }

    pub(crate) async fn preflight(
        &self,
        params: &ContainerCreateParams,
    ) -> DockerResult<PreparedAgentConfig> {
        let container_id = params
            .service_type
            .container_identifier(
                params.pod_id.as_deref(),
                params.user_id.as_deref(),
                params.project_id.as_deref(),
            )
            .map_err(|e| DockerError::ConfigurationError(e.to_string()))?
            .to_owned();
        if let Some(limits) = &params.resource_limits {
            limits.validate().map_err(|e| {
                DockerError::ConfigurationError(format!("Invalid resource limits: {e}"))
            })?;
        }
        let service_config = self
            .manager
            .get_service_config(&params.service_type)
            .await?;
        let image = self
            .manager
            .select_image(&params.service_type, None)
            .await?;
        Ok(PreparedAgentConfig {
            service_config,
            image,
            container_id,
        })
    }

    pub async fn start(&self, params: ContainerCreateParams) -> DockerResult<ContainerBasicInfo> {
        let prepared = self.preflight(&params).await?;
        self.start_prepared(params, prepared).await
    }

    async fn clear_previous_container_for_rebuild(&self, project_id: &str) -> DockerResult<()> {
        let Some(existing) = self.manager.get_container_info(project_id).await else {
            return Ok(());
        };
        let physical_id = &existing.container_id;
        if self
            .manager
            .find_container_authoritative(physical_id)
            .await?
            .is_some()
        {
            // Explicit rebuild retains force-delete semantics, unlike create's reuse path.
            self.manager.stop_container_by_id(physical_id).await?;
        }
        self.manager.retire_container_cache(physical_id).await
    }

    pub(crate) async fn start_prepared(
        &self,
        params: ContainerCreateParams,
        prepared: PreparedAgentConfig,
    ) -> DockerResult<ContainerBasicInfo> {
        let PreparedAgentConfig {
            service_config,
            image,
            container_id,
        } = prepared;
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

        // Preserve the existing rebuild behavior, but capture its physical target once.
        if project_id.is_some() {
            self.clear_previous_container_for_rebuild(&container_id)
                .await?;
        }

        use crate::container_builder::ContainerConfigBuilder;

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
            .auto_remove(true)
            // 结构化身份 label（对齐 K8s build_standard_labels / Docker Userapp
            // 容器的身份载体）：rcoder 重启后 Docker API list 按 label 还原身份
            // （消费侧暂未切换，铺重启窗口的 label 直读）
            .label("service-type", service_type.to_string())
            .label("identifier", container_id.clone());

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

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use container_runtime_api::{AgentContainerRuntime, ContainerRuntimeError};
    use std::sync::Arc;

    #[tokio::test]
    async fn prepared_rebuild_uses_canonical_pod_key_and_preserves_project_container() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        tokio::spawn(actor.run());
        let family = ServiceType::UserappBuilder;
        let mut config = crate::DockerManagerConfig::default();
        config.multi_image_config.services.insert(
            family.to_string(),
            serde_json::from_value(serde_json::json!({
                "service_type": family, "enabled":true, "image":"canonical-test-image"
            }))
            .unwrap(),
        );
        let manager = DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config,
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        };
        for (key, physical_id) in [
            ("project-one", "project-physical"),
            ("pod-one", "pod-physical"),
        ] {
            manager
                .containers
                .insert(
                    key.into(),
                    crate::DockerContainerInfo::new(
                        physical_id.into(),
                        format!("builder-{key}"),
                        key.into(),
                        "image".into(),
                    ),
                )
                .await;
            manager
                .api_cache
                .insert_network(physical_id.into(), Some(Arc::new(Default::default())))
                .await;
        }
        let server = tokio::spawn(async move {
            for phase in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let count = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(
                    request.contains("/containers/pod-physical"),
                    "rebuild must use preflight's canonical pod identity: {request}"
                );
                let (status, body) = if phase == 2 {
                    assert!(request.starts_with("DELETE "));
                    // Stop the real start_prepared chain at the selected mutation,
                    // before mounts, image downloads, or container creation.
                    (500, r#"{"message":"injected deletion failure"}"#)
                } else {
                    (
                        200,
                        r#"{"Id":"pod-physical","Name":"/builder-pod-one","State":{"Status":"running"}}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let params = ContainerCreateParams::builder()
            .project_id("project-one".to_string())
            .pod_id("pod-one")
            .service_type(family)
            .build();
        let starter = AgentContainerStarter::new(&manager);
        let prepared = starter.preflight(&params).await.unwrap();
        assert_eq!(prepared.container_id, "pod-one");
        let result = starter.start_prepared(params, prepared).await;
        server.await.unwrap();
        assert!(matches!(
            result,
            Err(DockerError::BollardError(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 500,
                    ..
                }
            ))
        ));
        assert_eq!(
            manager
                .containers
                .get("project-one")
                .await
                .unwrap()
                .container_id,
            "project-physical"
        );
    }

    #[tokio::test]
    async fn rebuild_keeps_replacement_registered_after_identity_capture() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        tokio::spawn(actor.run());
        let manager = Arc::new(DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig::default(),
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        });
        let old = crate::DockerContainerInfo::new(
            "old-id".into(),
            "builder".into(),
            "one".into(),
            "image".into(),
        );
        manager.containers.insert("one".into(), old.clone()).await;
        // Avoid an unrelated network request so the barrier targets the identity inspect.
        manager
            .api_cache
            .insert_network("old-id".into(), Some(Arc::new(Default::default())))
            .await;
        let writer = manager.clone();
        let server = tokio::spawn(async move {
            for phase in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let count = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(
                    request.contains("/containers/old-id"),
                    "must not retarget deletion using a mutable project key: {request}"
                );
                if phase == 0 {
                    let mut replacement = old.clone();
                    replacement.container_id = "new-id".into();
                    writer.containers.insert("one".into(), replacement).await;
                }
                let (status, body) = if phase == 2 {
                    assert!(request.starts_with("DELETE "));
                    assert!(
                        request.contains("force=true"),
                        "preserve explicit rebuild semantics"
                    );
                    (204, "")
                } else {
                    (
                        200,
                        r#"{"Id":"old-id","Name":"/builder","State":{"Status":"running"}}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        AgentContainerStarter::new(&manager)
            .clear_previous_container_for_rebuild("one")
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            manager.containers.get("one").await.unwrap().container_id,
            "new-id"
        );
    }

    #[tokio::test]
    async fn missing_builder_configuration_fails_before_operation_lease_or_docker_api() {
        let mut config = crate::DockerManagerConfig::default();
        config.multi_image_config.services.clear();
        let (_actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        let manager = DockerManager {
            docker: bollard::Docker::connect_with_http(
                "http://127.0.0.1:9",
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config,
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("unused".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(1, 1, 1)),
        };
        let runtime = crate::runtime::docker_runtime::DockerRuntime::new(Arc::new(manager));
        let params = ContainerCreateParams::builder()
            .project_id("preflight-test".to_string())
            .service_type(ServiceType::UserappBuilder)
            .build();
        let result = runtime.create_container(params).await;
        assert!(
            matches!(result, Err(ContainerRuntimeError::ConfigurationError(message)) if message.contains("not enabled"))
        );
    }
}

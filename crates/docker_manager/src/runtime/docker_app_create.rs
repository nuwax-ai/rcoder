//! Docker 侧 Userapp 部署**变更组**实现（create/patch/scale/recycle/restart/delete）。
//!
//! 方法体为 `DockerRuntime` 自有 impl（trait 壳在 docker_app_runtime.rs 一行委托——
//! 同一 trait 的 impl 块不可分割，k8s 侧 k8s_app_create.rs 同款模式）。

use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ExposeType,
    UserAppDeploymentRuntime,
};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::time::Duration;

use super::docker_app_mounts::build_prod_flat_mounts;
use super::docker_runtime::{
    APP_COMMAND_LABEL, APP_PORTS_LABEL, DockerRuntime, app_deployment_name, encode_ports_label,
    extract_container_ip,
};
use std::collections::HashMap;

struct PreparedAppContainer {
    app_id: String,
    image: String,
    container_name: String,
    main_network: String,
    config: bollard::models::ContainerCreateBody,
}

impl DockerRuntime {
    pub(crate) async fn create_deployment_impl(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let prepared = self.prepare_app_container(params).await?;
        self.create_prepared_app_container(prepared).await
    }

    async fn prepare_app_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<PreparedAppContainer> {
        use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};

        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires image_override".to_string(),
            )
        })?;
        if params.service_type != ServiceType::Userapp
            || app_id.trim().is_empty()
            || image.trim().is_empty()
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application creation requires UserApp identity and a nonempty image".into(),
            ));
        }
        self.inner
            .ensure_image_exists(&image)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "prepare application image {image}: {error}"
                ))
            })?;
        let container_name = app_deployment_name(&app_id);

        // env（env + secrets 合并；Docker 模式无 Secret 概念）
        let mut env_map: HashMap<String, String> = HashMap::new();
        if let Some(e) = &params.env {
            env_map.extend(e.clone());
        }
        if let Some(s) = &params.secrets {
            env_map.extend(s.clone());
        }
        // 平台注入 env（与压平挂载点绑定，覆盖用户 env——否则落镜像旧默认
        // /app/data 与发布卷耦合）。start-app.sh 均为 ${VAR:-...} 覆盖模式；
        // USERAPP_WORKSPACE_DIR 由镜像 supervisor conf 经 %(ENV_…)s 透传
        // （本地直跑无此 env 时镜像缺省回退 /app）。
        env_map.insert(
            "PGDATA".to_string(),
            shared_types::paths::USERAPP_DEV_PGDATA.to_string(),
        );
        env_map.insert(
            "DBX_DATA_DIR".to_string(),
            shared_types::paths::USERAPP_DEV_DBX_DATA.to_string(),
        );
        env_map.insert(
            "USERAPP_WORKSPACE_DIR".to_string(),
            format!("{}/{}", shared_types::paths::USERAPP_DEV_HOME, app_id),
        );
        // APP_ID：镜像 supervisor conf 经 %(ENV_APP_ID)s 消费（file-server 的
        // USERAPP_SINGLE_APP_ID），对齐 K8s 注入——Docker 缺失会让 supervisord
        // 插值失败直接拒启
        env_map.insert("APP_ID".to_string(), app_id.to_string());
        let env_vec: Vec<String> = env_map.iter().map(|(k, v)| format!("{k}={v}")).collect();

        // labels（供对账/list 过滤）
        let mut labels: HashMap<String, String> = HashMap::new();
        labels.insert("managed-by".to_string(), "rcoder-app-manager".to_string());
        labels.insert(
            shared_types::USERAPP_DOCKER_APP_ID_LABEL.to_string(),
            app_id.clone(),
        );
        labels.insert("service-type".to_string(), ServiceType::Userapp.to_string());
        if let Some(context) = &params.execution_context {
            context
                .validate_identity(&app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            labels.extend(context.resource_metadata());
        }
        if let Some(t) = &params.tenant_id {
            labels.insert("tenant".to_string(), t.clone());
        }
        if let Some(s) = &params.space_id {
            labels.insert("space".to_string(), s.clone());
        }
        // ports/command 元数据 label（update live 回退数据源）：Docker 侧 Http 走
        // Pingora 注册、Tcp 走 port_bindings，ExposedPorts 无法完整还原（Http 读不
        // 回、Tcp 被隐式 expose 后类型丢失、镜像 EXPOSE 幽灵端口混入）；command 无法
        // 区分用户显式设置与镜像 CMD 固化（inspect 的 Config.cmd 是合并结果）。两者
        // 用 label 显式持久化（与 K8s port-expose 注解同构），update 回退读回。
        if let Some(ports) = &params.ports
            && !ports.is_empty()
        {
            labels.insert(APP_PORTS_LABEL.to_string(), encode_ports_label(ports));
        }
        if let Some(command) = &params.command
            && !command.is_empty()
            && let Ok(encoded) = serde_json::to_string(command)
        {
            labels.insert(APP_COMMAND_LABEL.to_string(), encoded);
        }

        // TCP port_bindings（host_port=None 让 Docker 自动分配）
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        if let Some(ports) = &params.ports {
            for p in ports.iter().filter(|p| p.expose_type == ExposeType::Tcp) {
                port_bindings.insert(
                    format!("{}/tcp", p.port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None,
                    }]),
                );
            }
        }
        // deploy-host：Http 端口（app-entry 9080 等）一并发布——容器形态 Http
        // 走 Pingora 经容器 IP，宿主机形态 Pingora 数据面同样经注册表拨号，
        // 全部暴露端口需发布到宿主机
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host()
            && let Some(ports) = &params.ports
        {
            for p in ports.iter().filter(|p| p.expose_type == ExposeType::Http) {
                port_bindings
                    .entry(format!("{}/tcp", p.port))
                    .or_insert_with(|| {
                        Some(vec![PortBinding {
                            host_ip: Some("0.0.0.0".to_string()),
                            host_port: None,
                        }])
                    });
            }
        }

        // 挂载组装（prod 四目录压平，与 dev builder 同构）在 docker_app_mounts.rs——
        // 锚点反解 fail fast + 预创建 + 四 bind（恒四个，非空）。
        let mounts = Some(build_prod_flat_mounts(&app_id, params.user_id.as_deref()).await?);

        // 加入主网络（与 rcoder 同网络，Pingora 才能通过 container_ip 访问）
        // 同时保留网络名，供 start 后按网卡定位 container_ip（多网卡时避免 values().next() 取错）
        let main_network = self
            .inner
            .detect_main_network_name()
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!("prepare application network: {error}"))
            })?;
        let network_mode = Some(main_network.clone());

        let host_config = HostConfig {
            mounts,
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            network_mode,
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(image.clone()),
            cmd: params.command.clone(),
            env: if env_vec.is_empty() {
                None
            } else {
                Some(env_vec)
            },
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        };

        Ok(PreparedAppContainer {
            app_id,
            image,
            container_name,
            main_network,
            config,
        })
    }

    async fn create_prepared_app_container(
        &self,
        prepared: PreparedAppContainer,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        use bollard::query_parameters::{CreateContainerOptions, StartContainerOptions};
        let PreparedAppContainer {
            app_id,
            image,
            container_name,
            main_network,
            config,
        } = prepared;
        let client = self.inner.get_docker_client();
        let created = client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    platform: String::new(),
                }),
                config,
            )
            .await
            .map_err(|e| {
                // Fail Fast：打印 bollard 原始错误（含 daemon status_code/message），
                // 避免 service 层 context 吞掉根因（见 service.rs create_app 错误链）
                tracing::error!(
                    "[APP-DOCKER] create_container 失败 name={}, image={}: {e:?}",
                    container_name,
                    image
                );
                ContainerRuntimeError::ContainerCreationError(e.to_string())
            })?;
        if let Err(e) = client
            .start_container(&created.id, None::<StartContainerOptions>)
            .await
        {
            tracing::error!(
                "[APP-DOCKER] start_container 失败 name={}, id={}: {e:?}",
                container_name,
                created.id
            );
            // best-effort 强删已 created 的孤儿容器，避免残留导致下次同名创建冲突
            // （对齐 delete_deployment 的 force-remove 范式）
            use bollard::query_parameters::RemoveContainerOptions;
            if let Err(rm_e) = client
                .remove_container(
                    &created.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                tracing::debug!(
                    "[APP-DOCKER] best-effort remove orphan container {} failed: {}",
                    created.id,
                    rm_e
                );
            }
            return Err(ContainerRuntimeError::ContainerStartError(e.to_string()));
        }

        // 短轮询等待 container_ip 就绪（容器刚 start，IP 可能尚未分配）。
        // 优先取主网络网卡的 IP，回退任意网卡；最多重试 6 次 × 200ms。
        // deploy-host：同一 inspect 读回发布端口登记注册表（键 = container_name，
        // 与 funnel/清理路径同源）。
        let preferred = Some(main_network.as_str());
        let ip = {
            let mut ip = String::new();
            for attempt in 0..6u32 {
                match client.inspect_container(&created.id, None).await {
                    Ok(inspect) => {
                        #[cfg(feature = "deploy-host")]
                        if shared_types::is_deploy_host() {
                            let ports_ref = inspect
                                .network_settings
                                .as_ref()
                                .and_then(|ns| ns.ports.clone());
                            crate::deploy_host_ports::register_from_inspect(
                                &container_name,
                                inspect.name.as_deref(),
                                &ports_ref,
                            )
                            .map_err(|e| {
                                ContainerRuntimeError::DockerError(format!(
                                    "deploy-host app port registration failed: {e}"
                                ))
                            })?;
                        }
                        ip = extract_container_ip(&inspect, preferred);
                        if !ip.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[APP-DOCKER] inspect container {} for ip failed (attempt {attempt}): {}",
                            created.id,
                            e
                        );
                    }
                }
                if attempt < 5 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            ip
        };

        if ip.is_empty() {
            tracing::warn!(
                "[APP-DOCKER] container {} started but IP not ready after polling; \
                 Pingora/gRPC 注册前应确认可达，否则会掩盖启动故障",
                created.id
            );
        }
        Ok(ContainerBasicInfo {
            container_id: created.id.clone(),
            container_name,
            container_ip: ip,
            internal_port: 0,
            external_port: 0,
            project_id: app_id,
            status: "Running".to_string(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: None,
        })
    }

    /// 更新 Userapp 容器：Docker 不支持 in-place 改 image/env/command，必须重建。
    /// 完成配置、镜像、挂载与网络准备后，仅删除已验证归属的物理容器身份。
    /// 准备失败保留旧运行单元；删除失败中止，不吞错继续创建。
    pub(crate) async fn patch_deployment_impl(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let app_id = params.project_id.as_deref().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".into(),
            )
        })?;
        params.validate_execution_context()?;
        let snapshot = if let Some(context) = &params.execution_context {
            let target = match &params.mutation_target {
                Some(target) => target.clone(),
                None => self.capture_stop_target(context).await?,
            };
            if target.resource.kind != shared_types::AppResourceKind::Container
                || target.resource.name != app_deployment_name(app_id)
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Captured application update target changed".into(),
                ));
            }
            shared_types::AppDeletionSnapshot {
                app_id: app_id.into(),
                operation_id: context.operation_id.clone(),
                resources: vec![target.resource],
            }
        } else {
            self.capture_app_deletion(app_id, None).await?
        };
        let prepared = self.prepare_app_container(params).await.map_err(|error| {
            ContainerRuntimeError::PreparationFailed(shared_types::AppPreparationFailure {
                message: error.to_string(),
            })
        })?;
        self.delete_app_snapshot(&snapshot).await?;
        self.create_prepared_app_container(prepared).await
    }

    pub(crate) async fn scale_deployment_impl(
        &self,
        app_id: &str,
        replicas: i32,
    ) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        if !(0..=1).contains(&replicas) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Docker application replicas must be zero or one".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let name = match self.capture_app_container_id(app_id).await? {
            Some(id) => id,
            None if replicas == 0 => return Ok(()),
            None => {
                return Err(ContainerRuntimeError::ContainerStartError(
                    "Application container does not exist".into(),
                ));
            }
        };
        if replicas == 0 {
            // stop 幂等语义：容器已停（304）bollard 当成功；并发消失（404）容忍
            // ——stop 的目标态就是"不在跑"，容器没了目标态已达成（对齐
            // delete_deployment 的 404 容忍范式，竞态窗口不再 500）
            if let Err(e) = client
                .stop_container(
                    &name,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: Some(String::new()),
                    }),
                )
                .await
            {
                match e {
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: 304 | 404,
                        ..
                    } => {
                        tracing::debug!(
                            "[DOCKER] captured container {name} already stopped or absent"
                        );
                    }
                    other => {
                        return Err(ContainerRuntimeError::ContainerStopError(other.to_string()));
                    }
                }
            }
        } else {
            client
                .start_container(&name, None::<StartContainerOptions>)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        }
        Ok(())
    }

    pub(super) async fn capture_stop_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = app_deployment_name(&context.app_id);
        let container = self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "Inspect application stop target: {error}"
                ))
            })?;
        let uid = validate_app_container_target(&context.app_id, &container)?;
        let labels = container
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict(
                    "Application stop target has no identity labels".into(),
                )
            })?;
        let metadata = labels
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        context
            .validate_application_metadata(&metadata)
            .map_err(ContainerRuntimeError::Conflict)?;
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Container,
                name,
                uid,
                resource_version: None,
            },
        })
    }

    pub(super) async fn restart_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        image: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        match image {
            // Both calls retain the original physical ID. A failed stop does not
            // authorize starting another resource selected by the logical name.
            None => {
                self.stop_captured_target(target).await?;
                self.start_captured_target(target).await
            }
            // Image roll: Docker cannot swap an image in place. Prepare the
            // replacement from the verified live container, then delete and
            // recreate under the captured name (the prepare-then-delete
            // ordering of patch_deployment_impl).
            Some(image) => {
                self.recreate_captured_target_with_image(target, image)
                    .await
            }
        }
    }

    /// Recreate the captured container onto `image`. The create body is derived
    /// from the live container inspect so env/labels/cmd/mounts/network stay
    /// byte-identical (the PG-password retention guarantee); only the image
    /// differs. Any failure before the delete leaves the original running.
    pub(super) async fn recreate_captured_target_with_image(
        &self,
        target: &shared_types::UserAppMutationTarget,
        image: &str,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Container
            || target.resource.name != app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application restart target".into(),
            ));
        }
        let inspect = self
            .inner
            .get_docker_client()
            .inspect_container(&target.resource.uid, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "Inspect application restart target: {error}"
                ))
            })?;
        let uid = validate_app_container_target(&target.context.app_id, &inspect)?;
        if uid != target.resource.uid {
            return Err(ContainerRuntimeError::Conflict(
                "Captured application restart target changed".into(),
            ));
        }
        self.inner
            .ensure_image_exists(image)
            .await
            .map_err(|error| {
                ContainerRuntimeError::PreparationFailed(shared_types::AppPreparationFailure {
                    message: format!("prepare restart image {image}: {error}"),
                })
            })?;
        let live_config = inspect.config.clone().ok_or_else(|| {
            ContainerRuntimeError::DockerError("Restart target has no container config".into())
        })?;
        let host_config = inspect.host_config.clone();
        let inspected_network = host_config
            .as_ref()
            .and_then(|config| config.network_mode.clone())
            .filter(|mode| !mode.is_empty());
        let main_network = match inspected_network {
            Some(mode) => mode,
            None => self
                .inner
                .detect_main_network_name()
                .await
                .map_err(|error| {
                    ContainerRuntimeError::DockerError(format!(
                        "prepare application network: {error}"
                    ))
                })?,
        };
        let prepared = PreparedAppContainer {
            app_id: target.context.app_id.clone(),
            image: image.to_string(),
            container_name: target.resource.name.clone(),
            main_network,
            config: recreate_body_from_inspect(&live_config, host_config, image),
        };
        let snapshot = shared_types::AppDeletionSnapshot {
            app_id: target.context.app_id.clone(),
            operation_id: target.context.operation_id.clone(),
            resources: vec![target.resource.clone()],
        };
        self.delete_app_snapshot(&snapshot).await?;
        self.create_prepared_app_container(prepared).await?;
        Ok(())
    }

    pub(super) async fn start_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::StartContainerOptions;
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Container
            || target.resource.name != app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application start target".into(),
            ));
        }
        match self
            .inner
            .get_docker_client()
            .start_container(&target.resource.uid, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 304, ..
            }) => Ok(()),
            Err(error) => Err(ContainerRuntimeError::ContainerStartError(format!(
                "Start captured application: {error}"
            ))),
        }
    }

    pub(super) async fn captured_start_is_running(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        let current = self.capture_stop_target(&target.context).await?;
        if current != *target {
            return Err(ContainerRuntimeError::Conflict(
                "Acknowledged application start identity changed".into(),
            ));
        }
        let info = self
            .inner
            .get_docker_client()
            .inspect_container(&target.resource.uid, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "Confirm captured application start: {error}"
                ))
            })?;
        Ok(info.id.as_deref() == Some(target.resource.uid.as_str())
            && info
                .state
                .as_ref()
                .is_some_and(|state| state.running == Some(true) && state.restarting != Some(true)))
    }

    pub(super) async fn stop_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::StopContainerOptions;
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Container
            || target.resource.uid.is_empty()
            || target.resource.name != app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid physical application stop target".into(),
            ));
        }
        match self
            .inner
            .get_docker_client()
            .stop_container(
                &target.resource.uid,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 304 | 404,
                ..
            }) => Ok(()),
            Err(error) => {
                let message = format!("Stop captured application: {error}");
                if let bollard::errors::Error::DockerResponseServerError { status_code, .. } =
                    &error
                    && let Some(rejection) = shared_types::RuntimeRequestRejection::from_status(
                        *status_code,
                        message.clone(),
                    )
                {
                    return Err(ContainerRuntimeError::RequestRejected(rejection));
                }
                Err(ContainerRuntimeError::ContainerStopError(message))
            }
        }
    }

    async fn capture_app_container_id(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<String>> {
        let name = app_deployment_name(app_id);
        match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(container) => validate_app_container_target(app_id, &container).map(Some),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(ContainerRuntimeError::DockerError(format!(
                "Inspect application mutation target: {error}"
            ))),
        }
    }

    pub(crate) async fn restart_deployment_impl(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = self
            .capture_app_container_id(app_id)
            .await?
            .ok_or_else(|| {
                ContainerRuntimeError::ContainerStartError(
                    "Application container does not exist".into(),
                )
            })?;
        let client = self.inner.get_docker_client();
        // Restart must never start after an uncertain or rejected stop. Docker
        // already-stopped (304) is the only safe no-op response here.
        if let Err(error) = client
            .stop_container(
                &name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await
            && !matches!(
                &error,
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 304,
                    ..
                }
            )
        {
            return Err(ContainerRuntimeError::ContainerStopError(format!(
                "Stop captured application before restart: {error}"
            )));
        }
        client
            .start_container(&name, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        Ok(())
    }
}

fn validate_app_container_target(
    app_id: &str,
    container: &bollard::models::ContainerInspectResponse,
) -> ContainerRuntimeResult<String> {
    let labels = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| {
            ContainerRuntimeError::Conflict(
                "Application mutation target has no ownership labels".into(),
            )
        })?;
    if labels
        .get(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
        .map(String::as_str)
        != Some(app_id)
        || labels.get("service-type").map(String::as_str)
            != Some(ServiceType::Userapp.to_string().as_str())
        || labels.get("managed-by").map(String::as_str) != Some("rcoder-app-manager")
    {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutation target ownership changed".into(),
        ));
    }
    container
        .id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::DockerError(
                "Application mutation target has no physical container ID".into(),
            )
        })
}

/// Derive the recreate body from the live container inspect: env/labels/cmd and
/// the host config (mounts/network/port bindings) stay byte-identical — this is
/// the PG-password and workspace retention guarantee — and only the image is
/// replaced.
fn recreate_body_from_inspect(
    config: &bollard::models::ContainerConfig,
    host_config: Option<bollard::models::HostConfig>,
    image: &str,
) -> bollard::models::ContainerCreateBody {
    bollard::models::ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: config.cmd.clone(),
        env: config.env.clone(),
        labels: config.labels.clone(),
        host_config,
        ..Default::default()
    }
}

#[cfg(test)]
mod mutation_target_tests {
    use super::*;
    #[test]
    fn application_mutation_requires_physical_identity_and_correct_family() {
        let target: bollard::models::ContainerInspectResponse = serde_json::from_value(serde_json::json!({
            "Id":"physical-original", "Config":{"Labels":{
                (shared_types::USERAPP_DOCKER_APP_ID_LABEL):"app-one", "service-type":ServiceType::Userapp.to_string(), "managed-by":"rcoder-app-manager"
            }}
        })).expect("inspect");
        assert_eq!(
            validate_app_container_target("app-one", &target).expect("identity"),
            "physical-original"
        );
        assert!(validate_app_container_target("app-two", &target).is_err());
        let mut builder = target.clone();
        builder
            .config
            .as_mut()
            .expect("config")
            .labels
            .as_mut()
            .expect("labels")
            .insert(
                "service-type".into(),
                ServiceType::UserappBuilder.to_string(),
            );
        assert!(validate_app_container_target("app-one", &builder).is_err());
        let mut missing = target;
        missing.id = None;
        assert!(validate_app_container_target("app-one", &missing).is_err());
    }

    /// 反例锚定：重建体只换镜像——env/labels/cmd 与 HostConfig 必须逐字保留
    /// （PG 密码经 env 注入、工作区经 mounts 绑定，丢失即 CR10 破坏）。
    #[test]
    fn recreate_config_from_inspect_replaces_only_image() {
        let inspect: bollard::models::ContainerInspectResponse =
            serde_json::from_value(serde_json::json!({
                "Id": "physical-original",
                "Config": {
                    "Labels": {
                        (shared_types::USERAPP_DOCKER_APP_ID_LABEL): "app-one",
                        "service-type": ServiceType::Userapp.to_string(),
                        "managed-by": "rcoder-app-manager"
                    },
                    "Env": ["APP_ID=app-one", "PGPASSWORD=secret-value"],
                    "Cmd": ["/app/start-app.sh"]
                },
                "HostConfig": {
                    "NetworkMode": "rcoder-testnet",
                    "Binds": ["/tmp/ws-app-one:/home/user/app-one"]
                }
            }))
            .expect("inspect");
        let config = inspect.config.clone().expect("config");
        let host_config = inspect.host_config.clone();
        let body = recreate_body_from_inspect(
            &config,
            host_config.clone(),
            "registry.test/app-runtime:0.2.0",
        );
        assert_eq!(
            body.image.as_deref(),
            Some("registry.test/app-runtime:0.2.0")
        );
        assert_eq!(
            body.env.as_deref(),
            Some(
                [
                    "APP_ID=app-one".to_string(),
                    "PGPASSWORD=secret-value".to_string()
                ]
                .as_slice()
            ),
            "env must survive the image swap byte-identically"
        );
        assert_eq!(
            body.cmd.as_deref(),
            Some(["/app/start-app.sh".to_string()].as_slice())
        );
        let labels = body.labels.as_ref().expect("labels");
        assert_eq!(
            labels
                .get(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
                .map(String::as_str),
            Some("app-one")
        );
        let host = body.host_config.as_ref().expect("host config");
        assert_eq!(host.network_mode.as_deref(), Some("rcoder-testnet"));
        assert_eq!(
            host.binds.as_deref(),
            Some(["/tmp/ws-app-one:/home/user/app-one".to_string()].as_slice()),
            "workspace binds must survive the image swap"
        );
    }
}

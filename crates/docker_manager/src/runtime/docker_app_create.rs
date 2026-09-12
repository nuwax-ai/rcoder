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

use tracing::info;

use super::docker_app_mounts::build_prod_flat_mounts;
use super::docker_runtime::{
    APP_COMMAND_LABEL, APP_PORTS_LABEL, DockerRuntime, RecyclePolicy, app_deployment_name,
    encode_ports_label, extract_container_ip,
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
        let preferred = Some(main_network.as_str());
        let ip = {
            let mut ip = String::new();
            for attempt in 0..6u32 {
                match client.inspect_container(&created.id, None).await {
                    Ok(inspect) => {
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
        let snapshot = self.capture_app_deletion(app_id, None).await?;
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
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
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
                        status_code: 404, ..
                    } => {
                        tracing::debug!(
                            "[DOCKER] stop container {name} not found (raced removal?), idempotent ok"
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

    /// Docker 无 K8s 注解,改用内存态存储回收策略(merge 语义:None=不改该字段)。
    pub(crate) async fn patch_recycle_policy_impl(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        // merge 语义统一:Occupied 合并旧值,Vacant 以 default(全 None) 为基底 merge。
        // and_modify/or_insert_with 把两分支的 merge 语义收敛为一处表达。
        self.recycle_policy
            .entry(app_id.to_string())
            .and_modify(|p| *p = p.merge(recycle_enabled, idle_timeout_seconds))
            .or_insert_with(|| {
                RecyclePolicy::default().merge(recycle_enabled, idle_timeout_seconds)
            });
        info!(
            "[DOCKER-APP] recycle policy patched: {app_id} (enabled={:?}, idle_timeout={:?})",
            recycle_enabled, idle_timeout_seconds
        );
        Ok(())
    }

    pub(crate) async fn restart_deployment_impl(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        // best-effort: 容器可能已停止，忽略 stop 失败
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
            tracing::debug!(
                "[DOCKER] Best-effort stop container {} before restart failed: {}",
                name,
                e
            );
        }
        client
            .start_container(&name, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        Ok(())
    }
}

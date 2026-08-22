//! UserApp ContainerCreateParams 构建（从 service.rs 拆出，extension-impl）。
//!
//! build_container_params / _from_update / build_params_inner（create/update 共用）。

use std::collections::HashMap;

use tracing::warn;

use container_runtime_api::{
    AppHealthCheck, AppPortSpec, AppResourceRequirements, ContainerCreateParams, DeploymentStatus,
    ExposeType as RtExposeType,
};
use shared_types::ServiceType;

use crate::models::*;
use crate::release_flow::identity::{inject_release_identity, strip_release_identity};
use crate::service::AppService;
use crate::utils::*;

/// build_params_inner 的入参聚合（消除 too_many_arguments，create/update 两路统一填充）。
///
/// 各字段语义对齐 `CreateAppRequest` / `UpdateAppRequest` 的可选字段；owned（调用方填充，
/// inner 内部按需 move 进 ContainerCreateParams builder，无需重复 clone）。
struct AppParamsInput {
    image: String,
    command: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    secrets: Option<HashMap<String, String>>,
    ports: Option<Vec<PortConfig>>,
    health_check: Option<HealthCheckConfig>,
    resources: Option<ResourceLimits>,
    tenant_id: Option<String>,
    space_id: Option<String>,
    recycle_enabled: Option<bool>,
    idle_timeout_seconds: Option<u64>,
}

impl AppService {
    /// 构建 ContainerCreateParams（UserApp，create 路径）
    pub(crate) async fn build_container_params(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<ContainerCreateParams> {
        self.build_params_inner(
            app_id,
            AppParamsInput {
                // create 侧默认镜像已在 create_app_locked 单一收口填充（恒 Some）
                image: request.image.clone().unwrap_or_default(),
                command: request.command.clone(),
                env: request.env.clone(),
                secrets: request.secrets.clone(),
                ports: request.ports.clone(),
                health_check: request.health_check.clone(),
                resources: request.resources.clone(),
                tenant_id: request.tenant_id.clone(),
                space_id: request.space_id.clone(),
                recycle_enabled: request.recycle_enabled,
                idle_timeout_seconds: request.idle_timeout_seconds,
            },
        )
        .await
    }

    /// UpdateAppRequest → ContainerCreateParams（全量替换语义，image 必填）。
    /// image 缺失 → ERR_VALIDATION（rcoder 无状态，无法保留旧 image，调用方必须发完整新状态）。
    /// `current` 为 update 前的 DeploymentStatus（乐观锁用同一份），其 recycle 字段用于部分更新回填
    /// ——SSA re-apply 会擦除未携带的注解,故 `request.x.or(current.x)` 保留旧值(镜像 command/env 回退同因)。
    pub(crate) async fn build_container_params_from_update(
        &self,
        app_id: &str,
        request: &UpdateAppRequest,
        current: &DeploymentStatus,
    ) -> AppResult<ContainerCreateParams> {
        // image 缺失 = 平台默认运行时镜像（与 create 同源；无状态不持有旧值，
        // 缺省语义是"用当前默认"而非"保留旧值"）
        let image = match &request.image {
            Some(img) => img.clone(),
            None => default_runtime_image(&std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok())?,
        };
        // 部分更新回退（方案C 扩展）：`env`/`secrets`/`resources` 任一为 None 时从 live
        // 容器读当前值回退，避免部分更新静默清空：
        //   - `env` 丢 → K8s `cleanup_orphan_port_resources` 删 ConfigMap → 容器丢环境变量；
        //   - `secrets`/`resources` 丢 → K8s 从 Secret/pod limits 读回（Docker secrets
        //     不可分 → 恒 None 等价旧行为）。
        // `command`/`ports`/`health_check` **恒 live 回退**——v2 四要素平台内定（命令=manifest
        // 自动、HTTP 端口=pingap 9080 唯一、探针=app-cli 3010），UpdateAppRequest 已不再
        // 暴露这三字段（防调用方误传破坏发布链内定值），update 无权更改。
        let (command, env, secrets, resources, health_check, ports) = match self
            .runtime
            .get_app_container_spec(app_id)
            .await
        {
            Ok(spec) => (
                spec.command,
                // 读回的 env 必含 create 时注入的保留变量，先剥离再走 inject
                //（inject 从当前发布锁重新注入权威值；用户显式提交保留变量仍拒绝）。
                request.env.clone().or(spec.env.map(|mut env| {
                    strip_release_identity(&mut env);
                    env
                })),
                request.secrets.clone().or(spec.secrets),
                request
                    .resources
                    .clone()
                    .or(spec.resources.map(resource_limits_from_snapshot)),
                spec.health_check.map(health_check_from_snapshot),
                spec.ports
                    .map(|ps| ps.iter().map(port_config_from_snapshot).collect()),
            ),
            Err(e) => {
                warn!(
                    "[APP] get_app_container_spec failed app_id={app_id} (spec fields unavailable, update aborted): {e}"
                );
                // 读回失败=command/ports/health_check 无权威值可依——上抛而非带空值
                // SSA（空 ports 会触发 cleanup_orphan_port_resources 断掉对外入口）。
                return Err(AppOperationError::Backend(format!(
                    "cannot read live container spec for update: {e}"
                )));
            }
        };
        self.build_params_inner(
            app_id,
            AppParamsInput {
                image,
                command,
                env,
                secrets,
                ports,
                health_check,
                resources,
                tenant_id: request.tenant_id.clone(),
                space_id: request.space_id.clone(),
                recycle_enabled: request.recycle_enabled.or(current.recycle_enabled),
                idle_timeout_seconds: request
                    .idle_timeout_seconds
                    .or(current.idle_timeout_seconds),
            },
        )
        .await
    }

    /// build_container_params / build_container_params_from_update 的公共逻辑。
    ///
    /// 入参聚合为 `AppParamsInput`（owned），内部按需 move 进 ContainerCreateParams builder；
    /// 统一 create/update 两路逻辑：此前重复 ~180 行（90% 相同），分歧仅在 image 校验 +
    /// 问题④的 command/env 回退（均已在各自入口处理完毕，此处纯装配）。
    async fn build_params_inner(
        &self,
        app_id: &str,
        input: AppParamsInput,
    ) -> AppResult<ContainerCreateParams> {
        let AppParamsInput {
            image,
            command,
            env,
            secrets,
            ports,
            health_check,
            resources,
            tenant_id,
            space_id,
            recycle_enabled,
            idle_timeout_seconds,
        } = input;

        let app_dir = self.get_container_app_dir(app_id).await?;
        let env = inject_release_identity(&app_dir, env.unwrap_or_default()).await?;

        // 端口：models::PortConfig → container_runtime_api::AppPortSpec
        let app_ports: Vec<AppPortSpec> = ports
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortSpec {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        strip_prefix: p.strip_prefix,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Exec 健康检查当前未支持（AppHealthCheck 无 command 字段），Fail Fast 拒绝，
        // 避免静默丢弃用户配置（K8s build_probe 对 Exec 返回 None → 容器被视为永远健康）
        if let Some(hc) = &health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            return Err(AppOperationError::Validation(
                "Exec health check is not supported (AppHealthCheck lacks command field); use Http/Tcp instead"
                    .to_string(),
            ));
        }

        // 健康检查：models::HealthCheckConfig → AppHealthCheck
        let app_health_check = health_check.as_ref().map(|hc| AppHealthCheck {
            check_type: map_health_check_type(&hc.check_type),
            path: hc.path.clone(),
            liveness_path: hc.liveness_path.clone(),
            port: hc.port,
            initial_delay_seconds: None,
            period_seconds: None,
        });

        // 资源：models::ResourceLimits → AppResourceRequirements
        let app_resources = resources.as_ref().map(|r| AppResourceRequirements {
            cpu: r.cpu.clone(),
            memory: r.memory.clone(),
            storage: r.storage.clone(),
            ephemeral_storage: r.ephemeral_storage.clone(),
        });

        // 宿主机工作空间路径（Docker 模式 bind mount 源；K8s 模式 runtime 用 subPath，忽略此值）
        let host_workspace_path = self
            .get_host_app_dir(app_id)
            .await
            .to_string_lossy()
            .to_string();

        let mut builder = ContainerCreateParams::builder()
            .project_id(app_id.to_string())
            .service_type(ServiceType::UserApp)
            .host_workspace_path(host_workspace_path)
            .image_override(image)
            .env(env)
            .secrets(secrets.unwrap_or_default())
            .ports(app_ports);

        // command 仅在非空时设置（空 vec 会覆盖镜像 CMD）
        if let Some(cmd) = command
            && !cmd.is_empty()
        {
            builder = builder.command(cmd);
        }
        if let Some(hc) = app_health_check {
            builder = builder.health_check(hc);
        }
        if let Some(ar) = app_resources {
            // 阶段2: storage 落 per-app PVC requests.storage (CSI 服务端 subvolume 配额);
            // ephemeral_storage 仍限 overlay 可写层。
            if let Some(ss) = ar.storage.clone() {
                builder = builder.storage_size(ss);
            }
            builder = builder.app_resources(ar);
        }
        // tenant/space：进 ContainerCreateParams → build_app_labels 打 rcoder.io/tenant、
        // rcoder.io/space label（供对账/过滤）。
        if let Some(t) = tenant_id {
            builder = builder.tenant_id(t);
        }
        if let Some(s) = space_id {
            builder = builder.space_id(s);
        }
        // recycle 配置 → ContainerCreateParams → Deployment 注解(rcoder.io/recycle-enabled / idle-timeout-seconds)
        if let Some(re) = recycle_enabled {
            builder = builder.recycle_enabled(re);
        }
        if let Some(it) = idle_timeout_seconds {
            builder = builder.idle_timeout_seconds(it);
        }

        Ok(builder.build())
    }
}

/// 运行时端口快照 → models::PortConfig（update ports 回退；与 `map_expose_type` 互逆）。
fn port_config_from_snapshot(p: &AppPortSpec) -> PortConfig {
    PortConfig {
        name: p.name.clone(),
        port: p.port,
        expose_type: match p.expose_type {
            RtExposeType::Http => ExposeType::Http,
            RtExposeType::Tcp => ExposeType::Tcp,
        },
        strip_prefix: p.strip_prefix,
    }
}

/// 运行时资源快照 → models::ResourceLimits（update 回退：读回的 Quantity 字符串原样复用）。
fn resource_limits_from_snapshot(r: AppResourceRequirements) -> ResourceLimits {
    ResourceLimits {
        cpu: r.cpu,
        memory: r.memory,
        storage: r.storage,
        ephemeral_storage: r.ephemeral_storage,
    }
}

/// 运行时健康检查快照 → models::HealthCheckConfig（update 回退；
/// check_type 反向映射——两枚举 variants 一一对应）。
fn health_check_from_snapshot(hc: AppHealthCheck) -> HealthCheckConfig {
    let check_type = match hc.check_type {
        container_runtime_api::HealthCheckType::Http => HealthCheckType::Http,
        container_runtime_api::HealthCheckType::Tcp => HealthCheckType::Tcp,
        container_runtime_api::HealthCheckType::Exec => HealthCheckType::Exec,
        container_runtime_api::HealthCheckType::None => HealthCheckType::None,
    };
    HealthCheckConfig {
        check_type,
        path: hc.path,
        liveness_path: hc.liveness_path,
        port: hc.port,
    }
}

/// 解析平台默认运行时镜像（单一 app-runtime 镜像策略：测试/生产由部署 env 区分，
/// 与发布链 `ensure_app_runtime` 同读 `RCODER_RUNTIME_IMAGE_DIGEST`）。
/// env 未配置且调用方未显式传 image → Backend 错误（部署缺配置，fail fast）。
pub(crate) fn default_runtime_image(env_value: &Option<String>) -> AppResult<String> {
    env_value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AppOperationError::Backend(
                "image not provided and RCODER_RUNTIME_IMAGE_DIGEST env not set \
                 (platform default app-runtime image; deployment config missing)"
                    .to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UpdateAppRequest;
    use crate::test_support::{MockRuntime, release_lock, test_service};
    use container_runtime_api::{
        AppHealthCheck, AppResourceRequirements, ContainerSpecSnapshot, DeploymentStatus,
        HealthCheckType as RtHealthCheckType,
    };
    use std::sync::Arc;

    fn empty_update_request(image: &str) -> UpdateAppRequest {
        UpdateAppRequest {
            name: None,
            image: Some(image.to_owned()),
            env: None,
            secrets: None,
            resources: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        }
    }

    async fn service_with_release_lock(root: &std::path::Path, app_id: &str) -> AppService {
        let app_dir = root.join(app_id);
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        test_service(root, Arc::new(MockRuntime::default()))
    }

    /// 只传 image 的 update：secrets/resources/health_check 缺省时从 live 快照回退
    /// （不再静默清空），command/env 同理。
    #[tokio::test]
    async fn update_missing_fields_fall_back_to_live_spec() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let app_dir = root.path().join("app-fb");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        runtime.specs.insert(
            "app-fb".into(),
            ContainerSpecSnapshot {
                command: Some(vec![
                    "java".into(),
                    "-jar".into(),
                    "/app/code/app.jar".into(),
                ]),
                // 模拟真实后端读回：create 时注入的保留变量随 env 存进集群（ConfigMap/
                // 容器 Config），live 快照必含——不剥离会让 update 部分 update 必 400。
                env: Some(HashMap::from([
                    ("SPRING_PROFILES".into(), "prod".into()),
                    ("RCODER_PINGAP_VERSION".into(), "stale-from-cluster".into()),
                    ("RCODER_PINGAP_COMMIT".into(), "stale-commit".into()),
                    (
                        "RCODER_RUNTIME_IMAGE_DIGEST".into(),
                        "registry.example/app-runtime:OLD".into(),
                    ),
                ])),
                secrets: Some(HashMap::from([("DB_PASSWORD".into(), "s3cr3t".into())])),
                resources: Some(AppResourceRequirements {
                    cpu: Some("1".into()),
                    memory: Some("1Gi".into()),
                    storage: None,
                    ephemeral_storage: Some("2Gi".into()),
                }),
                health_check: Some(AppHealthCheck {
                    check_type: RtHealthCheckType::Http,
                    path: Some("/actuator/health".into()),
                    liveness_path: None,
                    port: Some(8080),
                    initial_delay_seconds: None,
                    period_seconds: None,
                }),
                // K8s 读回示意：container.ports + port-expose 注解（含 TCP 端口）
                ports: Some(vec![
                    AppPortSpec {
                        name: "http".into(),
                        port: 8080,
                        expose_type: RtExposeType::Http,
                        strip_prefix: None,
                    },
                    AppPortSpec {
                        name: "db".into(),
                        port: 5432,
                        expose_type: RtExposeType::Tcp,
                        strip_prefix: None,
                    },
                ]),
            },
        );
        let service = test_service(root.path(), runtime);

        let params = service
            .build_container_params_from_update(
                "app-fb",
                &empty_update_request("img:v2"),
                &DeploymentStatus::default(),
            )
            .await
            .expect("params with fallback");

        assert_eq!(
            params.command,
            Some(vec![
                "java".to_string(),
                "-jar".to_string(),
                "/app/code/app.jar".to_string()
            ])
        );
        // 业务 env 从 live 回退保留；保留变量被剥离后由 inject 从 release.lock.toml
        // 重新注入权威值（而非集群里的旧值）。
        let env = params.env.expect("env always set by builder");
        assert_eq!(env.get("SPRING_PROFILES").map(String::as_str), Some("prod"));
        assert_eq!(
            env.get("RCODER_PINGAP_VERSION").map(String::as_str),
            Some("0.13.7")
        );
        assert_eq!(
            env.get("RCODER_PINGAP_COMMIT").map(String::as_str),
            Some("abc123")
        );
        assert_eq!(
            env.get("RCODER_RUNTIME_IMAGE_DIGEST").map(String::as_str),
            Some("registry.example/app-runtime:0.1.140")
        );
        let secrets = params.secrets.expect("secrets fallback");
        assert_eq!(
            secrets.get("DB_PASSWORD").map(String::as_str),
            Some("s3cr3t")
        );
        let resources = params.app_resources.expect("resources fallback");
        assert_eq!(resources.cpu.as_deref(), Some("1"));
        assert_eq!(resources.memory.as_deref(), Some("1Gi"));
        assert_eq!(resources.ephemeral_storage.as_deref(), Some("2Gi"));
        let hc = params.health_check.expect("health_check fallback");
        assert!(matches!(hc.check_type, RtHealthCheckType::Http));
        assert_eq!(hc.path.as_deref(), Some("/actuator/health"));
        assert_eq!(hc.port, Some(8080));
        // ports 从 live 快照回退（此前缺省会清空全部对外端口）
        let ports = params.ports.expect("ports fallback");
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].port, 8080);
        assert!(matches!(ports[0].expose_type, RtExposeType::Http));
        assert_eq!(ports[0].name, "http");
        assert_eq!(ports[1].port, 5432);
        assert!(matches!(ports[1].expose_type, RtExposeType::Tcp));
    }

    /// 显式传值优先：请求携带的 secrets/resources 覆盖 live 快照（整段替换语义不变）。
    #[tokio::test]
    async fn update_explicit_fields_override_live_spec() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let app_dir = root.path().join("app-ov");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        runtime.specs.insert(
            "app-ov".into(),
            ContainerSpecSnapshot {
                command: None,
                env: None,
                secrets: Some(HashMap::from([("OLD".into(), "old".into())])),
                resources: Some(AppResourceRequirements {
                    cpu: Some("1".into()),
                    memory: None,
                    storage: None,
                    ephemeral_storage: None,
                }),
                health_check: None,
                ports: None,
            },
        );
        let service = test_service(root.path(), runtime);
        let mut request = empty_update_request("img:v2");
        request.secrets = Some(HashMap::from([("NEW".into(), "new".into())]));

        let params = service
            .build_container_params_from_update("app-ov", &request, &DeploymentStatus::default())
            .await
            .expect("params");

        let secrets = params.secrets.expect("explicit secrets");
        assert_eq!(secrets.get("NEW").map(String::as_str), Some("new"));
        assert!(
            !secrets.contains_key("OLD"),
            "explicit secrets replace live snapshot"
        );
    }

    /// 用户显式提交保留变量仍拒绝（防伪造语义不受回退剥离影响）。
    #[tokio::test]
    async fn update_explicit_reserved_env_still_rejected() {
        let root = tempfile::tempdir().expect("tempdir");
        let service = service_with_release_lock(root.path(), "app-reserved").await;
        let mut request = empty_update_request("img:v2");
        request.env = Some(HashMap::from([(
            "RCODER_PINGAP_VERSION".to_owned(),
            "user-value".to_owned(),
        )]));

        let error = service
            .build_container_params_from_update(
                "app-reserved",
                &request,
                &DeploymentStatus::default(),
            )
            .await
            .expect_err("explicit reserved env must fail");
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    /// live 快照缺字段（如 Docker 的 secrets/health_check 恒 None）→ 维持旧行为（空）。
    #[tokio::test]
    async fn update_fallback_absent_snapshot_field_stays_empty() {
        let root = tempfile::tempdir().expect("tempdir");
        let service = service_with_release_lock(root.path(), "app-empty").await;

        let params = service
            .build_container_params_from_update(
                "app-empty",
                &empty_update_request("img:v2"),
                &DeploymentStatus::default(),
            )
            .await
            .expect("params");

        assert!(
            params.secrets.as_ref().is_none_or(|m| m.is_empty()),
            "no snapshot → secrets empty (old behavior)"
        );
        assert!(params.app_resources.is_none());
        assert!(params.health_check.is_none());
    }

    #[cfg(test)]
    mod default_image {
        use super::default_runtime_image;

        #[test]
        fn env_present_resolves() {
            let img = default_runtime_image(&Some(" registry.example/app-runtime:0.1.9 ".into()))
                .expect("resolve");
            assert_eq!(img, "registry.example/app-runtime:0.1.9");
        }

        #[test]
        fn env_blank_is_missing() {
            assert!(default_runtime_image(&Some("   ".into())).is_err());
            assert!(default_runtime_image(&None).is_err());
        }
    }
}

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

use super::models::*;
use super::runtime_identity::{inject_release_identity, strip_release_identity};
use super::service::AppService;
use super::utils::*;

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
        // 部分更新回退（方案C 扩展）：`command`/`env`/`secrets`/`resources`/`health_check`/
        // `ports` 任一为 None 时从 live 容器读当前值回退，避免部分更新静默清空：
        //   - `command` 丢 → 镜像无 ENTRYPOINT 时 CrashLoop（container.args 为空）；
        //   - `env` 丢 → K8s `cleanup_orphan_port_resources` 删 ConfigMap → 容器丢环境变量；
        //   - `secrets`/`resources`/`health_check` 丢 → K8s 从 Secret/pod limits/probes 读回
        //     （Docker 的 secrets/health_check 不可分/无探针 → 恒 None 等价旧行为）；
        //   - `ports` 丢 → SSA 清 container ports + `cleanup_orphan_port_resources` 删
        //     HTTPRoute/NodePort → 对外入口全断（K8s 从 container.ports+port-expose 注解
        //     读回；Docker 从 ExposedPorts 尽力读回）。
        // 仅在确实缺省时才读（省一次后端 GET）；读失败降级为旧行为（清空）+ warn，不阻塞 update。
        // 注：`tenant_id`/`space_id` 仍为部分更新清空（在 K8s label 上，调用方携带即可还原）。
        let (command, env, secrets, resources, health_check, ports) = if request.command.is_none()
            || request.env.is_none()
            || request.secrets.is_none()
            || request.resources.is_none()
            || request.health_check.is_none()
            || request.ports.is_none()
        {
            match self.runtime.get_app_container_spec(app_id).await {
                Ok(spec) => (
                    request.command.clone().or(spec.command),
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
                    request
                        .health_check
                        .clone()
                        .or(spec.health_check.map(health_check_from_snapshot)),
                    request.ports.clone().or(spec
                        .ports
                        .map(|ps| ps.iter().map(port_config_from_snapshot).collect())),
                ),
                Err(e) => {
                    warn!(
                        "[APP] get_app_container_spec failed app_id={app_id} (missing fields may be cleared on partial update): {e}"
                    );
                    (
                        request.command.clone(),
                        request.env.clone(),
                        request.secrets.clone(),
                        request.resources.clone(),
                        request.health_check.clone(),
                        request.ports.clone(),
                    )
                }
            }
        } else {
            (
                request.command.clone(),
                request.env.clone(),
                request.secrets.clone(),
                request.resources.clone(),
                request.health_check.clone(),
                request.ports.clone(),
            )
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
pub(super) fn default_runtime_image(env_value: &Option<String>) -> AppResult<String> {
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
mod default_image_tests {
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

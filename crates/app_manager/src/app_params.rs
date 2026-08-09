//! UserApp ContainerCreateParams 构建（从 service.rs 拆出，extension-impl）。
//!
//! build_container_params / _from_update / build_params_inner（create/update 共用）。

use std::collections::HashMap;

use tracing::warn;

use container_runtime_api::{
    AppHealthCheck, AppPortSpec, AppResourceRequirements, ContainerCreateParams, DeploymentStatus,
};
use shared_types::ServiceType;

use super::models::*;
use super::runtime_identity::inject_release_identity;
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
                image: request.image.clone(),
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
        let image = request.image.clone().ok_or_else(|| {
            AppOperationError::Validation(
                "update requires image (rcoder is stateless, cannot retain previous image)"
                    .to_string(),
            )
        })?;
        // 问题④修复（方案C）：`command`/`env` 为 None 时从 live 容器读当前值回退（与 `ports`
        // 从运行时状态回退一致），避免部分更新静默清空：
        //   - `command` 丢 → 镜像无 ENTRYPOINT 时 CrashLoop（container.args 为空）；
        //   - `env` 丢 → K8s `cleanup_orphan_port_resources` 删 ConfigMap → 容器丢环境变量。
        // 仅在确实缺省时才读（省一次 K8s GET）；读失败降级为旧行为（清空）+ warn，不阻塞 update。
        // 注：`secrets`/`resources`/`health_check`/`tenant_id`/`space_id` 同为部分更新时静默清空，
        // 但 severity 较低（不直接 CrashLoop），暂不在此次修复范围（可同法扩展）。
        let (command, env) = if request.command.is_none() || request.env.is_none() {
            match self.runtime.get_app_container_spec(app_id).await {
                Ok(spec) => (
                    request.command.clone().or(spec.command),
                    request.env.clone().or(spec.env),
                ),
                Err(e) => {
                    warn!(
                        "[APP] get_app_container_spec failed app_id={app_id} (command/env may be cleared on partial update): {e}"
                    );
                    (request.command.clone(), request.env.clone())
                }
            }
        } else {
            (request.command.clone(), request.env.clone())
        };
        self.build_params_inner(
            app_id,
            AppParamsInput {
                image,
                command,
                env,
                secrets: request.secrets.clone(),
                ports: request.ports.clone(),
                health_check: request.health_check.clone(),
                resources: request.resources.clone(),
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

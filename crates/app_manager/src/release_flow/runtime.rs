//! 发布激活的运行时编排（自 rcoder/userapp_publish/app_lifecycle 下沉）：
//! 统一装配空运行单元参数；生命周期受理由部署协调器负责。
//!
//! 历史的 wait_app_ready（轮询 readiness 转-ready，300s）已退役：部署链的
//! 同步等待点前移到部署段完成（[`crate::lifecycle`] 的 wait_deploy_stage，
//! 轮询容器内 app-cli `/v1/deploy/status`）；readiness 探针仅负责 K8s 摘流。

use crate::models::commons::{ExposeType, HealthCheckType};
use crate::models::{AppOperationError, CreateAppRequest, HealthCheckConfig, PortConfig};
use crate::service::AppService;

/// app-runtime 容器公网端口（pingap 监听，对外 Service + PortConfig 用）。
/// 单一来源 shared_types::APP_ENTRY_PORT（dev 容器 manifest 流程、Pingora 免端口代理同值）。
const APP_HTTP_PORT: u16 = shared_types::APP_ENTRY_PORT;
/// app-cli 管理 API 端口（K8s 探针打这里：app-cli 自身提供 /health+/ready，不强依赖后端 app）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;
/// app-cli 提供的探针路径（liveness=进程活，readiness=初始化完成/可选桥接后端）。
const APP_LIVENESS_PATH: &str = "/health";
const APP_READINESS_PATH: &str = "/ready";

impl AppService {
    pub(crate) fn empty_runtime_request(
        &self,
        rcoder_app_id: &str,
        name: &str,
        deploy_env: Option<std::collections::HashMap<String, String>>,
        lifecycle_id: Option<&str>,
    ) -> Result<CreateAppRequest, AppOperationError> {
        let image = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").map_err(|_| {
            AppOperationError::Backend(
                "RCODER_RUNTIME_IMAGE_DIGEST env not set (app-runtime image for create_app)"
                    .to_string(),
            )
        })?;
        // 热部署令牌：创建时注入一次、恒定（后续 update 不覆盖——or_insert 语义
        // 不适用于此处：update 通道走 build_container_params_from_update 的 live
        // 回退，token 自然保留）。app-cli server 侧 /v1/deploy 鉴权消费。
        let mut env = deploy_env.unwrap_or_default();
        env.entry(shared_types::APP_DEPLOY_GENERATION_ID.into())
            .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string());
        env.entry("APP_CLI_DEPLOY_TOKEN".to_string())
            .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string());
        Ok(CreateAppRequest {
            app_id: Some(rcoder_app_id.to_string()),
            lifecycle_id: lifecycle_id.map(str::to_owned),
            request_id: None,
            name: name.to_string(),
            image: Some(image),
            command: None,
            env: Some(env),
            secrets: None,
            resources: None,
            ports: Some(vec![PortConfig {
                name: "http".to_string(),
                port: APP_HTTP_PORT,
                expose_type: ExposeType::Http,
                strip_prefix: None,
            }]),
            // 探针打 app-cli 的 3010 管理 API(非 pingap 9080):app-cli 自身提供 /health(liveness,
            // 进程活,后端有 bug 也不杀容器)+ /ready(readiness,默认 app-cli 就绪/可选桥接后端)。
            health_check: Some(HealthCheckConfig {
                check_type: HealthCheckType::Http,
                path: Some(APP_READINESS_PATH.to_string()),
                liveness_path: Some(APP_LIVENESS_PATH.to_string()),
                port: Some(APP_CLI_ADMIN_PORT),
            }),
            tenant_id: None,
            space_id: None,
            // 发布编排创建的 Userapp 默认参与闲置回收（= 免费用户语义）；如需付费常驻由调用方另行 update。
            recycle_enabled: None,
            idle_timeout_seconds: None,
        })
    }
}

//! 发布激活的运行时编排（自 rcoder/userapp_publish/app_lifecycle 下沉）：
//! ensure_app_runtime（幂等建/取运行单元）+ wait_app_ready（轮询就绪）。
//!
//! activate_release 单接口语义（切流→ensure 容器→等就绪→提交/失败）的组成部分；
//! 仅被 releases.rs 消费，不放 AppServiceTrait（内部实现细节）。

use std::time::Duration;

use tracing::info;

use super::models::commons::{AppStatus, ExposeType, HealthCheckType};
use super::models::{AppOperationError, CreateAppRequest, HealthCheckConfig, PortConfig};
use super::service::AppService;

/// app-runtime 容器公网端口（pingap 监听，对外 Service + PortConfig 用）。
const APP_HTTP_PORT: u16 = 9080;
/// app-cli 管理 API 端口（K8s 探针打这里：app-cli 自身提供 /health+/ready，不强依赖后端 app）。
const APP_CLI_ADMIN_PORT: u16 = 3010;
/// app-cli 提供的探针路径（liveness=进程活，readiness=初始化完成/可选桥接后端）。
const APP_LIVENESS_PATH: &str = "/health";
const APP_READINESS_PATH: &str = "/ready";
/// 就绪轮询间隔。
const READY_POLL_INTERVAL_SECS: u64 = 3;
/// 就绪等待默认超时秒数（activate 请求体 readinessTimeoutSeconds 可覆盖，范围 5..=1800）。
pub(crate) const DEFAULT_READY_TIMEOUT_SECS: u64 = 300;
/// 就绪等待超时上下限（与 build 超时 DEV_COMMAND_TIMEOUT_SECS=1800 对齐上限）。
pub(crate) const MIN_READY_TIMEOUT_SECS: u64 = 5;
pub(crate) const MAX_READY_TIMEOUT_SECS: u64 = 1800;

impl AppService {
    /// 确保 app 计算单元存在：不存在则 create_app（幂等；image/ports 首次设定后恒定）。
    /// 首次发布时 activate 切流先于本调用（app 尚不存在，激活序列跳过 stop/start）。
    ///
    /// `process_lock`：调用方已持有的该 app 进程级发布锁——create 分支走
    /// [`create_app_locked`]（已持锁内核），避免公共 `create_app` 的重入取锁死锁。
    pub(super) async fn ensure_app_runtime(
        &self,
        rcoder_app_id: &str,
        name: &str,
        process_lock: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), AppOperationError> {
        match self.get_app(rcoder_app_id).await {
            Ok(_) => {
                // app 已存在：image/ports/probes 首次设定后恒定，不自动 reconcile(#14)。
                // 注:app_service trait 只暴露运行时信息(AppRuntimeInfo,无 image 字段)，无法在此
                // 直接比对存储镜像;改为记录期望 image,平台升级 app-runtime 后运维可据日志发现滞后。
                info!(
                    app_id = %rcoder_app_id,
                    "[APP] app already exists; image/ports/probes are constant after first create \
                     and will NOT be reconciled to the desired image"
                );
                return Ok(());
            }
            Err(AppOperationError::NotFound(_)) => {} // 不存在 → create
            Err(e) => return Err(e),
        }
        let image = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").map_err(|_| {
            AppOperationError::Backend(
                "RCODER_RUNTIME_IMAGE_DIGEST env not set (app-runtime image for create_app)"
                    .to_string(),
            )
        })?;
        let request = CreateAppRequest {
            app_id: Some(rcoder_app_id.to_string()),
            name: name.to_string(),
            // 发布链 ensure 无 user 上下文:回填已存 metadata(Java 先 create 的场景),
            // 无值空串(record 侧转 None, 部署 URL 降级旧短形态)
            user_id: self
                .metadata
                .lookup(rcoder_app_id)
                .and_then(|m| m.user_id)
                .unwrap_or_default(),
            image: Some(image),
            command: None,
            env: None,
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
            // 发布编排创建的 UserApp 默认参与闲置回收（= 免费用户语义）；如需付费常驻由调用方另行 update。
            recycle_enabled: None,
            idle_timeout_seconds: None,
        };
        self.create_app_locked(rcoder_app_id, request, process_lock)
            .await
            .map(|_| ())
    }

    /// 轮询 app 到 status=Running 且 health 非 Unhealthy；超时或进入 Error 则失败
    /// （activate 就绪窗口）。超时秒数由调用方传入（activate 请求体，已校验范围）。
    ///
    /// 容错：后端瞬时错误（API 抖动/网络瞬断）在就绪预算内记日志继续轮询——单次
    /// 抖动不耗尽整个预算；但**连续** [`MAX_CONSECUTIVE_POLL_ERRORS`] 次失败判死
    /// （持续性故障如网络分区/RBAC 配错不该拖满整个预算才失败，最长 1800s）。
    /// `NotFound` 是"发布期间应用被用户删除"——等就绪阶段确实不持进程锁
    /// （activate_release 的 guard 被 ensure_app_runtime 按值消费、其返回即释放；
    /// 删除是更高优先级的用户意图），与普通就绪失败区分报错便于排查。
    pub(super) async fn wait_app_ready(
        &self,
        rcoder_app_id: &str,
        timeout_secs: u64,
    ) -> Result<(), AppOperationError> {
        const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 5;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let mut consecutive_errors = 0u32;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(format!(
                    "app readiness poll timed out after {timeout_secs}s"
                )));
            }
            match self.get_app(rcoder_app_id).await {
                Ok(info) => {
                    consecutive_errors = 0;
                    if info.status == AppStatus::Error {
                        return Err(AppOperationError::Backend(format!(
                            "app entered Error state (health={})",
                            info.health.status
                        )));
                    }
                    if info.status == AppStatus::Running && info.health.status != "Unhealthy" {
                        return Ok(());
                    }
                }
                Err(AppOperationError::NotFound(_)) => {
                    return Err(AppOperationError::Backend(format!(
                        "app {rcoder_app_id} was deleted while waiting for readiness"
                    )));
                }
                Err(error) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_POLL_ERRORS {
                        return Err(AppOperationError::Backend(format!(
                            "app readiness poll failed {consecutive_errors} consecutive \
                             times: {error}"
                        )));
                    }
                    tracing::warn!(
                        app_id = %rcoder_app_id,
                        attempt = consecutive_errors,
                        max = MAX_CONSECUTIVE_POLL_ERRORS,
                        %error,
                        "readiness poll transient error, retrying within budget"
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
        }
    }
}

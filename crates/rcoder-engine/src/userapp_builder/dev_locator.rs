//! `UserappDevLocator` 契约实现：app_manager 文件/存储八接口 `env=dev` 分支的
//! 开发容器定位回调（复用注册表 ensure + 探活自愈 + file-server 地址解析）。

use std::sync::{Arc, Weak};

use shared_types::ServiceType;

use super::{dev_file_server_addr, ensure_userapp_builder_probed};
use crate::app_state::AppState;

/// Weak 挂接 [`AppState`]——注入发生在 `AppState` Arc 包装后；Weak 防与
/// `AppState → app_service → dev_locator → AppState` 的引用环。
pub struct UserappDevLocator {
    state: Weak<AppState>,
}

impl UserappDevLocator {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self { state }
    }

    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .upgrade()
            .ok_or_else(|| "app state already dropped".to_string())
    }
}

#[async_trait::async_trait]
impl shared_types::UserappDevLocator for UserappDevLocator {
    async fn dev_file_server_addr(&self, app_id: &str) -> Result<String, String> {
        let state = self.state()?;
        // 低频管理面语义：先探活再返回（注册表命中死容器时自愈重建），
        // 与 pod ensure/keepalive 同款；热路径转发层不走这里（自有 30s 探活缓存）。
        let (info, created) = ensure_userapp_builder_probed(&state, app_id)
            .await
            .map_err(|e| format!("ensure UserappBuilder (app {app_id}): {e:#}"))?;
        if created {
            tracing::info!("[USERAPP_DEV_LOCATOR] builder ensured on demand: app_id={app_id}");
        }
        dev_file_server_addr(&state, &info).map_err(|error| error.to_string())
    }

    async fn dev_logs_file_server_addr(&self, app_id: &str) -> Result<String, String> {
        let state = self.state()?;
        let actual = state
            .runtime()
            .find_container(app_id, &ServiceType::UserappBuilder)
            .await
            .map_err(|error| format!("observe development log container: {error}"))?
            .ok_or_else(|| format!("Development container for app {app_id} does not exist; its file logs are currently unavailable"))?;
        super::validate_builder_identity(app_id, &actual).map_err(|error| error.to_string())?;
        if actual.status != container_runtime_api::ContainerRuntimeStatus::Running {
            return Err(format!(
                "Development container for app {app_id} is not running; its file logs are currently unavailable"
            ));
        }
        super::adoption::verify_live_builder(&state, app_id, app_id, &actual.container_id)
            .await
            .map_err(|error| format!("verify development log container identity: {error:#}"))?;
        let info = state
            .runtime()
            .get_container_info_by_identifier(app_id, &ServiceType::UserappBuilder)
            .await
            .map_err(|error| format!("observe development file-server address: {error}"))?
            .ok_or_else(|| format!("Development file-server for app {app_id} is unavailable"))?;
        if info.container_id != actual.container_id || info.workload_uid != actual.workload_uid {
            return Err(
                "Development container changed while locating logs; retry the query".into(),
            );
        }
        dev_file_server_addr(&state, &info).map_err(|error| error.to_string())
    }

    async fn dev_container_alive(&self, app_id: &str) -> Result<bool, String> {
        let state = self.state()?;
        state
            .runtime()
            .find_container(app_id, &ServiceType::UserappBuilder)
            .await
            .map(|found| found.is_some())
            .map_err(|e| format!("find UserappBuilder (app {app_id}): {e}"))
    }
}

/// 终端代理（rcoder-proxy）的 dev 容器懒启动回调：容器不在时自动 ensure
/// 创建。应用共享：按 app_id 定位（URL 用户占位段不参与）。
#[async_trait::async_trait]
impl shared_types::UserappDevEnsure for UserappDevLocator {
    async fn locate_dev_builder(
        &self,
        app_id: &str,
    ) -> Result<Option<shared_types::DevBuilderInstance>, shared_types::DevEnsureError> {
        let state = self
            .state()
            .map_err(|error| shared_types::DevEnsureError::ObserveFailed {
                app_id: app_id.to_string(),
                detail: error,
            })?;
        let found = state
            .runtime()
            .find_container(app_id, &ServiceType::UserappBuilder)
            .await
            .map_err(|error| shared_types::DevEnsureError::ObserveFailed {
                app_id: app_id.to_string(),
                detail: format!("find UserappBuilder: {error}"),
            })?;
        let Some(info) = found else {
            return Ok(None);
        };
        if info.container_ip.is_empty() {
            // 权威存在但无地址（Pending/重启窗口）：不可拨流也不可 ensure 绕过，
            // 交调用方等待语义（P0 总预算内重试）。
            return Err(shared_types::DevEnsureError::NotReady {
                app_id: app_id.to_string(),
                detail: format!("builder present without address (status {:?})", info.status),
            });
        }
        Ok(Some(shared_types::DevBuilderInstance {
            address: info.container_ip,
            container_id: info.container_id,
        }))
    }

    async fn ensure_dev_container(
        &self,
        app_id: &str,
    ) -> Result<shared_types::ContainerBasicInfo, shared_types::DevEnsureError> {
        let state = self
            .state()
            .map_err(|error| shared_types::DevEnsureError::EnsureFailed {
                app_id: app_id.to_string(),
                detail: error,
            })?;
        let (info, created) = ensure_userapp_builder_probed(&state, app_id)
            .await
            .map_err(|error| classify_ensure_error(app_id, &error))?;
        if created {
            tracing::info!("[USERAPP_DEV_LOCATOR] builder ensured on demand: app_id={app_id}");
        }
        Ok(info)
    }
}

/// ensure 链 anyhow 错误 → 类型化失败。以链上保留的 `AppOperationError`
/// 类型根判别（非字符串猜测）；未识别的根按执行故障兜底。
fn classify_ensure_error(app_id: &str, error: &anyhow::Error) -> shared_types::DevEnsureError {
    use app_manager::models::AppOperationError;
    let app_id = app_id.to_string();
    if let Some(operation_error) = error.downcast_ref::<AppOperationError>() {
        return classify_app_operation_error(app_id, operation_error);
    }
    shared_types::DevEnsureError::EnsureFailed {
        app_id,
        detail: format!("{error:#}"),
    }
}

fn classify_app_operation_error(
    app_id: String,
    error: &app_manager::models::AppOperationError,
) -> shared_types::DevEnsureError {
    use app_manager::models::AppOperationError;
    match error {
        AppOperationError::NotFound(_) => shared_types::DevEnsureError::BuilderAbsent { app_id },
        AppOperationError::Conflict(_)
        | AppOperationError::ConflictBlocked { .. }
        | AppOperationError::OperationInProgress { .. } => {
            shared_types::DevEnsureError::OperationInFlight {
                app_id,
                detail: error.to_string(),
            }
        }
        // 受理回执包一层操作身份后内嵌原始拒绝——递归按内层判别。
        AppOperationError::Operation { source, .. } => classify_app_operation_error(app_id, source),
        AppOperationError::Diagnostic(wake_failure) => {
            // wake 准入被 blocker 阻塞视作围栏；其余 wake 失败按执行故障。
            if wake_failure.blocker.is_some() {
                shared_types::DevEnsureError::OperationInFlight {
                    app_id,
                    detail: wake_failure.to_string(),
                }
            } else {
                shared_types::DevEnsureError::EnsureFailed {
                    app_id,
                    detail: wake_failure.to_string(),
                }
            }
        }
        _ => shared_types::DevEnsureError::EnsureFailed {
            app_id,
            detail: error.to_string(),
        },
    }
}

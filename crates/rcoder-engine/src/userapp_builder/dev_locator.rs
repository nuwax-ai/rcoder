//! `UserappDevLocator` 契约实现：app_manager 文件/存储八接口 `env=dev` 分支的
//! 开发容器定位回调；只读观察与显式 ensure/唤醒保留独立入口。

#![deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]

use std::sync::{Arc, Weak};

use shared_types::ServiceType;

use super::{dev_file_server_addr, ensure_userapp_builder_probed};
use crate::app_state::AppState;

mod observation;

#[cfg(test)]
#[path = "dev_locator_tests.rs"]
mod tests;

/// Weak 挂接 [`AppState`]——注入发生在 `AppState` Arc 包装后；Weak 防与
/// `AppState → app_service → dev_locator → AppState` 的引用环。
pub struct UserappDevLocator {
    state: Weak<AppState>,
}

enum VerifiedDevBuilder {
    Absent,
    /// Captured compute is retained and actually stopped; ensure may wake it.
    Stopped,
    NotAddressable,
    Present(Box<container_runtime_api::RuntimeContainerInfo>),
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

    /// Recreation asks whether old compute remains, not for an Active address.
    /// Keep its Deleted tombstone as the capture identity and only prove absence
    /// from stable physical observations. This never ensures or adopts compute.
    async fn deleted_builder_exists(
        state: &AppState,
        app: &shared_types::UserAppLifecycleRecord,
    ) -> anyhow::Result<bool> {
        use container_runtime_api::UserAppRuntimeReadiness;
        use shared_types::{UserAppExecutionContext, UserAppNoComputeState, UserappStage};

        let context = UserAppExecutionContext {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: "read-only-recreation".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        let before = super::adoption::capture_bound_target(state, &context).await?;
        before.validate().map_err(anyhow::Error::msg)?;
        let readiness = state
            .runtime()
            .observe_userapp_readiness(&app.app_id, UserappStage::Dev)
            .await?;
        let after = super::adoption::capture_bound_target(state, &context).await?;
        after.validate().map_err(anyhow::Error::msg)?;
        let current = state.userapp_store.get_application(&app.app_id).await?;
        if current.as_ref() != Some(app)
            || before.context != context
            || after.context != context
            || !observation::same_read_capture(&before, &after)
        {
            return Err(anyhow::anyhow!(
                "Application or builder changed during recreation verification; retry the query"
            ));
        }
        if after.workload.is_some() || after.pod.is_some() {
            // A retained zero-replica controller is compute that must settle
            // through its original cleanup; Missing discovery is insufficient.
            return Ok(true);
        }
        if readiness == UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Missing) {
            return Ok(false);
        }
        Err(anyhow::anyhow!(
            "Builder absence is unconfirmed during recreation verification"
        ))
    }

    /// Discovery parameters are filters, not an ownership proof. Compare two
    /// structured observations, then verify the captured physical resource and
    /// lifecycle before publishing the address. This path never ensures compute.
    async fn verified_builder(
        &self,
        state: &AppState,
        app_id: &str,
    ) -> anyhow::Result<VerifiedDevBuilder> {
        let Some(observed) = state
            .runtime()
            .find_container(app_id, &ServiceType::UserappBuilder)
            .await?
        else {
            return Self::verify_without_address(state, app_id).await;
        };
        super::validate_builder_identity(app_id, &observed)?;
        let current = state
            .runtime()
            .find_container(app_id, &ServiceType::UserappBuilder)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Builder disappeared during discovery"))?;
        super::validate_builder_identity(app_id, &current)?;
        if !observation::same_discovery_endpoint(&observed, &current)
            || current.status != observed.status
        {
            return Err(anyhow::anyhow!(
                "Builder instance or address changed during discovery; retry the query"
            ));
        }
        // The final capture closes replacement between the address reads and
        // validates the durable binding; a later runtime change cannot be made
        // atomic with a network dial, so callers retain normal retry semantics.
        super::adoption::verify_live_builder_observation(state, app_id, app_id, &current).await?;
        if !observation::is_running(&current.status) {
            // Some runtimes return a retained stopped container rather than
            // None. Only actual read-only compute observation permits wake.
            return Self::verify_without_address(state, app_id).await;
        }
        Ok(VerifiedDevBuilder::Present(Box::new(current)))
    }

    /// A missing Pod lookup can hide a retained controller. Its actual replica
    /// observation must agree with bound captures before and after the read.
    async fn verify_without_address(
        state: &AppState,
        app_id: &str,
    ) -> anyhow::Result<VerifiedDevBuilder> {
        use container_runtime_api::UserAppRuntimeReadiness;
        use shared_types::{UserAppNoComputeState, UserappStage};

        if state.userapp_store.get_application(app_id).await?.is_none() {
            let observation = state
                .runtime()
                .observe_userapp_readiness(app_id, UserappStage::Dev)
                .await?;
            if observation == UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Missing)
                && state.userapp_store.get_application(app_id).await?.is_none()
            {
                return Ok(VerifiedDevBuilder::Absent);
            }
            return Err(anyhow::anyhow!(
                "Builder exists or its absence is unconfirmed without an application lifecycle"
            ));
        }

        let captured = super::adoption::verified_live_builder_target(state, app_id, app_id).await?;
        let observation = state
            .runtime()
            .observe_userapp_readiness(app_id, UserappStage::Dev)
            .await?;
        let current = super::adoption::verified_live_builder_target(state, app_id, app_id).await?;
        if !observation::same_read_capture(&captured, &current) {
            return Err(anyhow::anyhow!(
                "Builder changed while verifying missing address; retry the query"
            ));
        }
        match observation {
            UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Missing)
                if current.workload.is_none() =>
            {
                Ok(VerifiedDevBuilder::Absent)
            }
            UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Stopped)
                if current.workload.is_some() && current.pod.is_none() =>
            {
                Ok(VerifiedDevBuilder::Stopped)
            }
            UserAppRuntimeReadiness::NotRunning(
                UserAppNoComputeState::Starting
                | UserAppNoComputeState::Stopping
                | UserAppNoComputeState::Failed,
            ) if current.workload.is_some() => Ok(VerifiedDevBuilder::NotAddressable),
            UserAppRuntimeReadiness::Running(target)
                if readiness_matches_capture(&target, &current) =>
            {
                // The separate runtime query found compute, while discovery
                // still lacks its structured address. Keep retry semantics.
                Ok(VerifiedDevBuilder::NotAddressable)
            }
            UserAppRuntimeReadiness::NotRunning(
                UserAppNoComputeState::Missing
                | UserAppNoComputeState::Starting
                | UserAppNoComputeState::Stopping
                | UserAppNoComputeState::Stopped
                | UserAppNoComputeState::Failed
                | UserAppNoComputeState::Unknown,
            )
            | UserAppRuntimeReadiness::Running(_) => Err(anyhow::anyhow!(
                "Builder absence or readiness observation does not match the captured identity"
            )),
        }
    }
}

fn readiness_matches_capture(
    observed: &container_runtime_api::UserAppReadinessTarget,
    captured: &shared_types::BuilderControlTarget,
) -> bool {
    use container_runtime_api::UserAppReadinessInstance;
    if observed.app_id != captured.context.app_id {
        return false;
    }
    match observed.stage {
        shared_types::UserappStage::Dev => {}
        shared_types::UserappStage::Prod => return false,
    }
    match &observed.instance {
        UserAppReadinessInstance::Docker { container_id, .. } => {
            captured.pod.is_none()
                && captured.workload.as_ref().is_some_and(|workload| {
                    workload.kind == shared_types::AppResourceKind::Container
                        && workload.uid == *container_id
                })
        }
        UserAppReadinessInstance::Kubernetes {
            pod_uid, owner_uid, ..
        } => {
            captured.pod.as_ref().is_some_and(|pod| pod.uid == *pod_uid)
                && captured.workload.as_ref().is_some_and(|workload| {
                    workload.kind == shared_types::AppResourceKind::StatefulSet
                        && workload.uid == *owner_uid
                })
        }
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
        let observed = self
            .verified_builder(&state, app_id)
            .await
            .map_err(|error| format!("observe development log container: {error:#}"))?;
        let actual = match observed {
            VerifiedDevBuilder::Present(actual) => actual,
            VerifiedDevBuilder::Absent => {
                return Err(format!(
                    "Development container for app {app_id} does not exist; its file logs are currently unavailable"
                ));
            }
            VerifiedDevBuilder::Stopped | VerifiedDevBuilder::NotAddressable => {
                return Err(format!(
                    "Development container for app {app_id} is not running; its file logs are currently unavailable"
                ));
            }
        };
        if !observation::is_running(&actual.status) {
            return Err(format!(
                "Development container for app {app_id} is not running; its file logs are currently unavailable"
            ));
        }
        let info = state
            .runtime()
            .get_container_info_by_identifier(app_id, &ServiceType::UserappBuilder)
            .await
            .map_err(|error| format!("observe development file-server address: {error}"))?
            .ok_or_else(|| format!("Development file-server for app {app_id} is unavailable"))?;
        if !observation::same_log_endpoint(&actual, &info) {
            return Err(
                "Development container changed while locating logs; retry the query".into(),
            );
        }
        dev_file_server_addr(&state, &info).map_err(|error| error.to_string())
    }

    async fn dev_container_alive(&self, app_id: &str) -> Result<bool, String> {
        let state = self.state()?;
        if let Some(app) = state
            .userapp_store
            .get_application(app_id)
            .await
            .map_err(|error| format!("read builder lifecycle (app {app_id}): {error}"))?
        {
            match app.state {
                shared_types::UserAppLifecycleState::Deleted => {
                    return Self::deleted_builder_exists(&state, &app)
                        .await
                        .map_err(|error| {
                            format!("verify deleted UserappBuilder (app {app_id}): {error:#}")
                        });
                }
                shared_types::UserAppLifecycleState::Active
                | shared_types::UserAppLifecycleState::Deleting => {}
            }
        }
        self.verified_builder(&state, app_id)
            .await
            .map(|found| match found {
                VerifiedDevBuilder::Absent => false,
                VerifiedDevBuilder::Stopped
                | VerifiedDevBuilder::NotAddressable
                | VerifiedDevBuilder::Present(_) => true,
            })
            .map_err(|e| format!("find UserappBuilder (app {app_id}): {e:#}"))
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
        let found = self
            .verified_builder(&state, app_id)
            .await
            .map_err(|error| shared_types::DevEnsureError::ObserveFailed {
                app_id: app_id.to_string(),
                detail: format!("verify UserappBuilder: {error:#}"),
            })?;
        let info = match found {
            VerifiedDevBuilder::Absent | VerifiedDevBuilder::Stopped => return Ok(None),
            VerifiedDevBuilder::NotAddressable => {
                return Err(shared_types::DevEnsureError::NotReady {
                    app_id: app_id.to_string(),
                    detail: "builder compute is present without a verified discovery address"
                        .into(),
                });
            }
            VerifiedDevBuilder::Present(info) => info,
        };
        if !observation::is_running(&info.status) || info.container_ip.is_empty() {
            // 权威存在但无地址（Pending/重启窗口）：不可拨流也不可 ensure 绕过，
            // 交调用方等待语义（P0 总预算内重试）。
            return Err(shared_types::DevEnsureError::NotReady {
                app_id: app_id.to_string(),
                detail: format!("builder is not addressable (status {:?})", info.status),
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
        AppOperationError::OperationNotFound(_)
        | AppOperationError::AlreadyExists(_)
        | AppOperationError::InvalidState(_)
        | AppOperationError::FileNotFound(_)
        | AppOperationError::Validation(_)
        | AppOperationError::DevNotRunning(_)
        | AppOperationError::Backend(_)
        | AppOperationError::CredentialApplication { .. }
        | AppOperationError::RuntimeRejected(_)
        | AppOperationError::HotDeployEnvChange(_) => shared_types::DevEnsureError::EnsureFailed {
            app_id,
            detail: error.to_string(),
        },
    }
}

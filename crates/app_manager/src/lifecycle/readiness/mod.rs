//! 业务就绪查询只读合并平台控制意图和实际实例的 app-cli 快照。
//! 不申请操作锁，不刷新闲置计时，也不推进恢复/启停。观察不是准入依据。
//!
//! 结构：本文件是 service 查询面（reader/prober 装配、预算编排、
//! 存储控制读取）；纯状态推导在 [`merge`]，dbx 查询路径在 [`dbx`]。

mod dbx;
mod merge;

#[cfg(test)]
mod dbx_query_tests;

use std::sync::Arc;
use std::time::Duration;

use shared_types::{
    ComputeControlAction, UserAppLifecycleState, UserAppOperationScope,
    UserAppReadinessObservation, UserAppReadinessResponse, UserappStage,
};
use tokio::time::Instant;

use crate::error::AppOperationError;
use crate::models::AppResult;
use crate::service::AppService;
use crate::utils::validate_app_id;

use merge::{ControlIntent, ReadinessControl, merge_observation};

pub(crate) const READINESS_QUERY_BUDGET: Duration = Duration::from_secs(8);
const DBX_READINESS_QUERY_BUDGET: Duration = Duration::from_secs(3);

impl AppService {
    pub fn set_readiness_reader(
        &self,
        reader: Arc<dyn shared_types::UserAppReadinessReader>,
    ) -> AppResult<()> {
        let mut slot = self
            .readiness_reader
            .write()
            .map_err(|_| AppOperationError::Backend("readiness reader lock poisoned".into()))?;
        *slot = Some(reader);
        Ok(())
    }

    pub(crate) fn readiness_reader(&self) -> Option<Arc<dyn shared_types::UserAppReadinessReader>> {
        self.readiness_reader
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    pub fn set_dbx_prober(
        &self,
        prober: Arc<dyn shared_types::DbxReadinessProber>,
    ) -> AppResult<()> {
        let mut slot = self
            .dbx_prober
            .write()
            .map_err(|_| AppOperationError::Backend("dbx prober lock poisoned".into()))?;
        *slot = Some(prober);
        Ok(())
    }

    /// dbx-web（容器内恒起 :4224）只读就绪查询：不唤醒、不建 dev 容器、
    /// 不刷新闲置计时（对齐业务 readiness 族"观察不是准入"的语义）。
    pub async fn get_app_dbx_readiness(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<shared_types::DbxReadinessResponse> {
        validate_app_id(app_id)?;
        let prober = self
            .dbx_prober
            .read()
            .map_err(|_| AppOperationError::Backend("dbx prober lock poisoned".into()))?
            .clone();
        dbx::query_dbx_with_budget(
            app_id,
            app_stage,
            DBX_READINESS_QUERY_BUDGET,
            || self.read_readiness_control(app_id, app_stage),
            prober,
        )
        .await
    }

    pub async fn get_app_readiness(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<UserAppReadinessResponse> {
        self.get_readiness_with_budget(app_stage, app_id, READINESS_QUERY_BUDGET)
            .await
    }

    async fn get_readiness_with_budget(
        &self,
        stage: UserappStage,
        app_id: &str,
        budget: Duration,
    ) -> AppResult<UserAppReadinessResponse> {
        validate_app_id(app_id)?;
        let deadline = Instant::now() + budget;
        // Keep completed authoritative reads if the next I/O exhausts the
        // shared deadline. A slow app-cli cannot erase an accepted Restart.
        let mut latest_control = ReadinessControl::ordinary(None);
        let observe = async {
            let Some(reader) = self.readiness_reader() else {
                return Err(AppOperationError::Backend(
                    "readiness reader is not wired (host assembly missing)".into(),
                ));
            };
            let mut before = self.read_readiness_control(app_id, stage).await?;
            latest_control = before.clone();
            for attempt in 0..2 {
                let observation = reader
                    .observe(
                        app_id,
                        stage,
                        deadline.saturating_duration_since(Instant::now()),
                    )
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!(
                            "Readiness observation (app {app_id}): {error}"
                        ))
                    })?;
                // Stop/Restart use independent compute controls, not business
                // slots. Reread both after I/O so an old ready cannot hide Stop.
                let after = self.read_readiness_control(app_id, stage).await?;
                latest_control = after.clone();
                if before.lifecycle_id != after.lifecycle_id {
                    return Ok(merge_observation(
                        app_id,
                        stage,
                        &after,
                        UserAppReadinessObservation::InstanceChanged,
                    ));
                }
                if before == after || after.intent == ControlIntent::StopAccepted {
                    return Ok(merge_observation(app_id, stage, &after, observation));
                }
                if attempt == 1 {
                    return Ok(merge_observation(
                        app_id,
                        stage,
                        &after,
                        UserAppReadinessObservation::InstanceChanged,
                    ));
                }
                before = after;
            }
            Ok(merge_observation(
                app_id,
                stage,
                &before,
                UserAppReadinessObservation::InstanceChanged,
            ))
        };
        // Covers storage reads too; observation timeout cannot mutate lifecycle
        // or turn an unknown business result into Failed/RecoveryRequired.
        match tokio::time::timeout_at(deadline, observe).await {
            Ok(result) => result,
            Err(_) => Ok(merge_observation(
                app_id,
                stage,
                &latest_control,
                UserAppReadinessObservation::TimedOut,
            )),
        }
    }

    async fn read_readiness_control(
        &self,
        app_id: &str,
        stage: UserappStage,
    ) -> AppResult<ReadinessControl> {
        let record = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .filter(|record| record.state != UserAppLifecycleState::Deleted)
            .ok_or_else(|| AppOperationError::NotFound(format!("app {app_id} not found")))?;
        let scope = match stage {
            UserappStage::Dev => UserAppOperationScope::Dev,
            UserappStage::Prod => UserAppOperationScope::Prod,
        };
        let operation = match record.active_operations.slot(scope) {
            Some(id) => self
                .metadata
                .store
                .get_operation(app_id, id)
                .await?
                .filter(|operation| {
                    operation.lifecycle_id == record.lifecycle_id && !operation.state.is_terminal()
                }),
            None => None,
        };
        let mut control = ReadinessControl::ordinary(operation.as_ref());
        control.lifecycle_id = record.lifecycle_id.clone();
        control.compute = self
            .metadata
            .store
            .read_compute_status(app_id, &record.lifecycle_id, scope)
            .await?;
        if let Some(compute) = control
            .compute
            .operation
            .as_ref()
            .filter(|operation| !operation.state.is_terminal())
        {
            control.intent = match compute.action {
                ComputeControlAction::Stop => ControlIntent::StopAccepted,
                ComputeControlAction::Restart => ControlIntent::Restarting,
            };
            control.operation_id = Some(compute.operation_id.clone());
        }
        if record.state == UserAppLifecycleState::Deleting {
            control.intent = ControlIntent::StopAccepted;
        }
        Ok(control)
    }
}

#[cfg(test)]
mod tests {
    use super::merge::fixtures::ready_snapshot;
    use super::*;
    use shared_types::UserAppLifecycleStore;
    use shared_types::{
        ComputeControlState, UserAppContainerStatus, UserAppReadinessPhysical,
        UserAppReadinessReason, UserAppReadinessStatus,
    };

    #[tokio::test]
    async fn query_rereads_compute_stop_admitted_during_observation_and_isolates_prod() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Reader {
            store: Arc<dyn UserAppLifecycleStore>,
            request: shared_types::ComputeControlRequest,
            first: AtomicBool,
        }
        #[async_trait::async_trait]
        impl shared_types::UserAppReadinessReader for Reader {
            async fn observe(
                &self,
                _: &str,
                _: UserappStage,
                _: Duration,
            ) -> Result<UserAppReadinessObservation, String> {
                if self.first.swap(false, Ordering::SeqCst) {
                    self.store
                        .admit_compute_control(&self.request)
                        .await
                        .map_err(|err| err.to_string())?;
                }
                Ok(UserAppReadinessObservation::Snapshot {
                    physical: UserAppReadinessPhysical {
                        instance_id: Some("old-pod".into()),
                        address: None,
                        channel: shared_types::UserAppReadinessChannel::Exec,
                    },
                    snapshot: ready_snapshot(),
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        let identity = store.ensure_identity("194").await.unwrap();
        service
            .set_readiness_reader(Arc::new(Reader {
                store: store.clone(),
                first: AtomicBool::new(true),
                request: shared_types::ComputeControlRequest {
                    app_id: "194".into(),
                    lifecycle_id: identity.lifecycle_id,
                    scope: UserAppOperationScope::Dev,
                    operation_id: "stopdev".into(),
                    request_id: "stoprequest".into(),
                    request_fingerprint: "a".repeat(64),
                    action: ComputeControlAction::Stop,
                    restart_image_roll: false,
                },
            }))
            .unwrap();
        let response = service
            .get_app_readiness(UserappStage::Dev, "194")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Stopping);
        assert_eq!(response.container.status, UserAppContainerStatus::Stopping);
        assert_eq!(
            response.container.operation.as_ref().unwrap().operation_id,
            "stopdev"
        );
        assert!(!response.ready);
        assert_eq!(response.operation_id.as_deref(), Some("stopdev"));
        let response = service
            .get_app_readiness(UserappStage::Prod, "194")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Ready);
        assert_eq!(response.container.status, UserAppContainerStatus::Running);
        assert!(response.container.operation.is_none());
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            store
                .get_compute_control("194", "stopdev")
                .await
                .unwrap()
                .unwrap()
                .state,
            ComputeControlState::Pending
        );
    }

    #[tokio::test]
    async fn readiness_deadline_preserves_accepted_restart_without_waking_compute() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct StalledReader(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl shared_types::UserAppReadinessReader for StalledReader {
            async fn observe(
                &self,
                _: &str,
                _: UserappStage,
                _: Duration,
            ) -> Result<UserAppReadinessObservation, String> {
                self.0.store(true, Ordering::SeqCst);
                std::future::pending().await
            }
        }
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        let app = store.ensure_identity("194").await.unwrap();
        let accepted = store
            .admit_compute_control(&shared_types::ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id,
                scope: UserAppOperationScope::Prod,
                operation_id: "restartpending".into(),
                request_id: "restartpending".into(),
                request_fingerprint: "b".repeat(64),
                action: ComputeControlAction::Restart,
                restart_image_roll: true,
            })
            .await
            .unwrap();
        let reached = Arc::new(AtomicBool::new(false));
        service
            .set_readiness_reader(Arc::new(StalledReader(reached.clone())))
            .unwrap();
        let response = service
            .get_readiness_with_budget(UserappStage::Prod, "194", Duration::from_millis(250))
            .await
            .unwrap();
        assert!(
            reached.load(Ordering::SeqCst),
            "the timeout must occur in the runtime observation"
        );
        assert!(!response.ready);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ObserveIncomplete)
        );
        assert_eq!(
            response.container.status,
            UserAppContainerStatus::Restarting
        );
        assert_eq!(
            response.container.operation.as_ref().unwrap().operation_id,
            accepted.operation_id
        );
        assert_eq!(
            store
                .get_compute_control("194", &accepted.operation_id)
                .await
                .unwrap(),
            Some(accepted)
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    }
}

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
                    snapshot: Box::new(ready_snapshot()),
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
                    builder_startup_mode: Default::default(),
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
                builder_startup_mode: Default::default(),
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

    /// 服务层证明「单一窗口」：dev `ensure_builder` 受理后，无论容器缺失、
    /// 拉起中还是 admin 未应答，顶层恒为 `provisioning`；app-cli 空 owner
    /// 快照一旦出现立即接管为 `not_deployed`；dev 槽不影响 prod 槽；全程
    /// 只读（不唤醒、不建容器）。
    #[tokio::test]
    async fn dev_builder_slot_reports_compute_provisioning() {
        use std::sync::atomic::{AtomicU8, Ordering};

        use shared_types::{
            UserAppAdmissionOutcome, UserAppBusinessReadiness, UserAppNoComputeState,
            UserAppOperationKind, UserAppProxyReadiness,
        };

        fn empty_owner_snapshot() -> UserAppBusinessReadiness {
            UserAppBusinessReadiness {
                ready: false,
                status: UserAppReadinessStatus::NotDeployed,
                reason_code: None,
                checked_at: "2026-10-19T08:00:00Z".into(),
                runtime_instance_id: None,
                serving_release_id: None,
                target_release_id: None,
                operation_id: None,
                observation_revision: 2,
                proxy: UserAppProxyReadiness {
                    confirmation: None,
                    ready: false,
                    status: UserAppReadinessStatus::NotDeployed,
                    reason_code: None,
                    error_origin_contract: None,
                },
                services: Vec::new(),
            }
        }

        struct Reader(AtomicU8);
        #[async_trait::async_trait]
        impl shared_types::UserAppReadinessReader for Reader {
            async fn observe(
                &self,
                _: &str,
                stage: UserappStage,
                _: Duration,
            ) -> Result<UserAppReadinessObservation, String> {
                if stage == UserappStage::Prod {
                    return Ok(UserAppReadinessObservation::NoCompute {
                        state: UserAppNoComputeState::Missing,
                    });
                }
                match self.0.fetch_add(1, Ordering::SeqCst) {
                    // 资源申请窗口的三跳物理事实：缺失 → 拉起中 → admin 未应答。
                    0 | 1 => Ok(UserAppReadinessObservation::NoCompute {
                        state: UserAppNoComputeState::Missing,
                    }),
                    2 => Ok(UserAppReadinessObservation::NoCompute {
                        state: UserAppNoComputeState::Starting,
                    }),
                    3 => Ok(UserAppReadinessObservation::AdminUnreachable {
                        physical: UserAppReadinessPhysical {
                            instance_id: Some("builder-pod".into()),
                            address: Some("127.0.0.1:3010".into()),
                            channel: shared_types::UserAppReadinessChannel::Exec,
                        },
                    }),
                    // 窗口结束：app-cli 空 owner 快照接管。
                    _ => Ok(UserAppReadinessObservation::Snapshot {
                        physical: UserAppReadinessPhysical {
                            instance_id: None,
                            address: None,
                            channel: shared_types::UserAppReadinessChannel::Exec,
                        },
                        snapshot: Box::new(empty_owner_snapshot()),
                    }),
                }
            }
        }

        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        let app = store.ensure_identity("49").await.unwrap();
        service
            .set_readiness_reader(Arc::new(Reader(AtomicU8::new(0))))
            .unwrap();

        // 基线：无在途操作，dev/prod 均如实 not_deployed。
        for stage in [UserappStage::Dev, UserappStage::Prod] {
            let response = service.get_app_readiness(stage, "49").await.unwrap();
            assert_eq!(response.status, UserAppReadinessStatus::NotDeployed);
        }

        let admitted = match store
            .admit(&shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: "ensurebuilder".into(),
                request_id: Some("ensurebuilderreq".into()),
                request_fingerprint: "c".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            UserAppAdmissionOutcome::Existing(_) => panic!("fresh admission expected"),
        };
        assert_eq!(admitted.scope, UserAppOperationScope::Dev);

        // 窗口第一帧：provisioning，计算事实如实（missing、无 compute 回执、
        // 无业务服务、无实例身份）。
        let response = service
            .get_app_readiness(UserappStage::Dev, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Provisioning);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ComputeProvisioning)
        );
        assert!(!response.ready);
        assert_eq!(response.operation_id.as_deref(), Some("ensurebuilder"));
        assert_eq!(response.container.status, UserAppContainerStatus::Missing);
        assert!(response.container.operation.is_none());
        assert!(response.services.is_empty());
        assert_eq!(response.runtime_instance_id, None);
        assert_eq!(response.observation_revision, 0);

        // 容器拉起中：仍是 provisioning，不冒充业务 starting。
        let response = service
            .get_app_readiness(UserappStage::Dev, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Provisioning);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ComputeProvisioning)
        );
        assert_eq!(response.container.status, UserAppContainerStatus::Starting);

        // builder 进程尚未应答 admin：仍是 provisioning（单一窗口性质）。
        let response = service
            .get_app_readiness(UserappStage::Dev, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Provisioning);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ComputeProvisioning)
        );
        assert_eq!(response.container.status, UserAppContainerStatus::Running);

        // dev 槽的 builder 操作不影响 prod 槽：同 app 的 prod 仍 not_deployed。
        let response = service
            .get_app_readiness(UserappStage::Prod, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::NotDeployed);

        // prod 槽受理业务启动后：那是 starting，不是 provisioning。
        match store
            .admit(&shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: "startprod".into(),
                request_id: Some("startprodreq".into()),
                request_fingerprint: "d".repeat(64),
                kind: UserAppOperationKind::Start,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(_) => {}
            UserAppAdmissionOutcome::Existing(_) => panic!("fresh admission expected"),
        }
        let response = service
            .get_app_readiness(UserappStage::Prod, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Starting);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ServiceStarting)
        );

        // 窗口结束：app-cli 空 owner 快照胜出，即使 dev 槽仍被 EnsureBuilder
        // 占用（provisioning 无后置覆盖）。
        let response = service
            .get_app_readiness(UserappStage::Dev, "49")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::NotDeployed);
        assert_eq!(response.observation_revision, 2);

        // 只读性：整个窗口查询不产生任何 runtime 写调用。
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    }
}

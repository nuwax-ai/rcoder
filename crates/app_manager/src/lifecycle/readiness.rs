//! 业务就绪查询只读合并平台控制意图和实际实例的 app-cli 快照。
//! 不申请操作锁，不刷新闲置计时，也不推进恢复/启停。观察不是准入依据。

use std::sync::Arc;
use std::time::Duration;

use shared_types::{
    ComputeControlAction, UserAppLifecycleState, UserAppNoComputeState, UserAppOperationKind,
    UserAppOperationRecord, UserAppOperationScope, UserAppProxyReadiness,
    UserAppReadinessObservation, UserAppReadinessReason, UserAppReadinessResponse,
    UserAppReadinessStatus, UserappStage,
};
use tokio::time::Instant;

use crate::error::AppOperationError;
use crate::models::AppResult;
use crate::service::AppService;
use crate::utils::validate_app_id;

pub(crate) const READINESS_QUERY_BUDGET: Duration = Duration::from_secs(8);

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
        let observe = async {
            let Some(reader) = self.readiness_reader() else {
                return Err(AppOperationError::Backend(
                    "readiness reader is not wired (host assembly missing)".into(),
                ));
            };
            let mut before = self.read_readiness_control(app_id, stage).await?;
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
                &ReadinessControl::ordinary(None),
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
        if let Some(compute) = self
            .metadata
            .store
            .active_compute_controls(app_id)
            .await?
            .into_iter()
            .filter(|control| {
                control.lifecycle_id == record.lifecycle_id
                    && control.scope == scope
                    && !control.state.is_terminal()
            })
            .max_by_key(|control| control.generation)
        {
            control.intent = match compute.action {
                ComputeControlAction::Stop => ControlIntent::StopAccepted,
                ComputeControlAction::Restart => ControlIntent::Restarting,
            };
            control.operation_id = Some(compute.operation_id);
        }
        control.desired_stopped = self
            .metadata
            .store
            .compute_desired_stopped(app_id, &record.lifecycle_id, scope)
            .await?;
        if record.state == UserAppLifecycleState::Deleting {
            control.intent = ControlIntent::StopAccepted;
        }
        Ok(control)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlIntent {
    None,
    StopAccepted,
    Starting,
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadinessControl {
    lifecycle_id: String,
    intent: ControlIntent,
    operation_id: Option<String>,
    desired_stopped: bool,
}

impl ReadinessControl {
    fn ordinary(operation: Option<&UserAppOperationRecord>) -> Self {
        let intent = match operation.map(|operation| operation.kind) {
            Some(
                UserAppOperationKind::Stop
                | UserAppOperationKind::StopBuilder
                | UserAppOperationKind::DeleteCompute,
            ) => ControlIntent::StopAccepted,
            Some(
                UserAppOperationKind::Restart
                | UserAppOperationKind::RestartDeployment
                | UserAppOperationKind::RestartBuilder,
            ) => ControlIntent::Restarting,
            Some(
                UserAppOperationKind::Create
                | UserAppOperationKind::StartDeployment
                | UserAppOperationKind::Start
                | UserAppOperationKind::EnsureBuilder
                | UserAppOperationKind::Update
                | UserAppOperationKind::HotDeploy,
            ) => ControlIntent::Starting,
            _ => ControlIntent::None,
        };
        Self {
            lifecycle_id: String::new(),
            intent,
            operation_id: operation.map(|op| op.operation_id.clone()),
            desired_stopped: false,
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn merge_observation(
    app_id: &str,
    app_stage: UserappStage,
    control: &ReadinessControl,
    observation: UserAppReadinessObservation,
) -> UserAppReadinessResponse {
    use UserAppReadinessReason as Reason;
    use UserAppReadinessStatus as Status;
    let base = |status: Status, reason: Option<Reason>| UserAppReadinessResponse {
        app_id: app_id.into(),
        app_stage: app_stage.as_str().into(),
        ready: false,
        status,
        reason_code: reason,
        checked_at: now_rfc3339(),
        runtime_instance_id: None,
        serving_release_id: None,
        target_release_id: None,
        operation_id: control.operation_id.clone(),
        runtime_operation_id: None,
        observation_revision: 0,
        proxy: UserAppProxyReadiness {
            ready: false,
            status,
            reason_code: reason,
            error_origin_contract: None,
        },
        services: Vec::new(),
    };
    let starting = matches!(
        control.intent,
        ControlIntent::Starting | ControlIntent::Restarting
    );
    let mut response = match observation {
        UserAppReadinessObservation::InstanceChanged => {
            base(Status::Unknown, Some(Reason::InstanceChanged))
        }
        UserAppReadinessObservation::TimedOut => {
            base(Status::Unknown, Some(Reason::ObserveIncomplete))
        }
        UserAppReadinessObservation::UnsupportedRuntime { .. } => {
            base(Status::Unsupported, Some(Reason::RuntimeUpgradeRequired))
        }
        UserAppReadinessObservation::AdminUnreachable { .. } => {
            if starting {
                base(Status::Starting, Some(Reason::ServiceStarting))
            } else {
                base(Status::Unknown, Some(Reason::AdminUnreachable))
            }
        }
        UserAppReadinessObservation::NoCompute { state } => {
            let status = match state {
                UserAppNoComputeState::Starting => Status::Starting,
                UserAppNoComputeState::Stopping => Status::Stopping,
                UserAppNoComputeState::Unknown => Status::Unknown,
                UserAppNoComputeState::Missing
                | UserAppNoComputeState::Stopped
                | UserAppNoComputeState::Failed
                    if control.desired_stopped =>
                {
                    Status::Stopped
                }
                UserAppNoComputeState::Missing | UserAppNoComputeState::Stopped if starting => {
                    Status::Starting
                }
                UserAppNoComputeState::Missing => Status::NotDeployed,
                UserAppNoComputeState::Stopped => Status::Stopped,
                UserAppNoComputeState::Failed => Status::Failed,
            };
            let reason = match status {
                Status::Starting => Some(Reason::ServiceStarting),
                Status::Stopping => Some(Reason::StopAccepted),
                Status::Failed => Some(Reason::OrchestrationFailed),
                Status::Unknown => Some(Reason::ObserveIncomplete),
                _ => None,
            };
            base(status, reason)
        }
        UserAppReadinessObservation::Snapshot { snapshot, .. } => UserAppReadinessResponse {
            app_id: app_id.into(),
            app_stage: app_stage.as_str().into(),
            ready: snapshot.ready && snapshot.status.is_ready(),
            status: snapshot.status,
            reason_code: snapshot.reason_code,
            checked_at: snapshot.checked_at,
            runtime_instance_id: snapshot.runtime_instance_id,
            serving_release_id: snapshot.serving_release_id,
            target_release_id: snapshot.target_release_id,
            operation_id: control.operation_id.clone(),
            runtime_operation_id: snapshot.operation_id,
            observation_revision: snapshot.observation_revision,
            proxy: snapshot.proxy,
            services: snapshot.services,
        },
    };
    // Only an in-flight Stop overrides a live snapshot. A historical desired
    // state helps classify a removed container, but must not mask a later
    // manually started owner or pretend a completed/failed Stop is still active.
    if control.intent == ControlIntent::StopAccepted {
        response.ready = false;
        response.status = Status::Stopping;
        response.reason_code = Some(Reason::StopAccepted);
    } else if control.intent == ControlIntent::Restarting {
        response.ready = false;
        response.status = Status::Starting;
        response.reason_code = Some(Reason::ServiceStarting);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{
        UserAppBusinessReadiness, UserAppOperationState, UserAppReadinessChannel,
        UserAppReadinessPhysical,
    };

    fn operation(kind: UserAppOperationKind) -> UserAppOperationRecord {
        UserAppOperationRecord {
            runtime_policy_on_success: None,
            command: None,
            admitted_metadata: None,
            operation_id: "op-1".into(),
            app_id: "194".into(),
            lifecycle_id: "life".into(),
            request_id: Some("req".into()),
            request_fingerprint: "0".repeat(64),
            kind,
            scope: UserAppOperationScope::Prod,
            state: UserAppOperationState::Running,
            revision: 1,
            executor_id: None,
            step: "executing".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn ready_snapshot() -> UserAppBusinessReadiness {
        UserAppBusinessReadiness {
            ready: true,
            status: UserAppReadinessStatus::Ready,
            reason_code: None,
            checked_at: "2026-09-26T08:00:00Z".into(),
            runtime_instance_id: Some("instance-1".into()),
            serving_release_id: Some("rel-1".into()),
            target_release_id: None,
            operation_id: Some("runtime-op-1".into()),
            observation_revision: 5,
            proxy: UserAppProxyReadiness {
                ready: true,
                status: UserAppReadinessStatus::Ready,
                reason_code: None,
                error_origin_contract: Some("pingap_etype_v1".into()),
            },
            services: Vec::new(),
        }
    }

    /// Stop 已受理但物理实例未退出：旧 ready 不可沿（§4.4 关键窗口）。
    #[test]
    fn stop_accepted_downgrades_stale_ready_to_stopping() {
        let stop = operation(UserAppOperationKind::Stop);
        let snapshot = ready_snapshot();
        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(Some(&stop)),
            UserAppReadinessObservation::Snapshot {
                physical: UserAppReadinessPhysical {
                    instance_id: Some("pod-uid".into()),
                    address: None,
                    channel: UserAppReadinessChannel::Direct,
                },
                snapshot,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopping);
        assert!(!response.ready);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::StopAccepted)
        );
        // 平台操作与 app-cli 操作分字段透出
        assert_eq!(response.operation_id.as_deref(), Some("op-1"));
        assert_eq!(
            response.runtime_operation_id.as_deref(),
            Some("runtime-op-1")
        );
    }

    /// 无控制意图时业务快照原样透出（ready 保留）。
    #[test]
    fn snapshot_without_control_intent_passes_through() {
        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(None),
            UserAppReadinessObservation::Snapshot {
                physical: UserAppReadinessPhysical {
                    instance_id: None,
                    address: None,
                    channel: UserAppReadinessChannel::Direct,
                },
                snapshot: ready_snapshot(),
            },
        );
        assert!(response.ready);
        assert_eq!(response.status, UserAppReadinessStatus::Ready);
        assert_eq!(response.observation_revision, 5);
        assert_eq!(
            response.proxy.error_origin_contract.as_deref(),
            Some("pingap_etype_v1")
        );
    }

    /// 计算缺失细节 × 控制意图矩阵：stopping/stopped/not_deployed/starting。
    #[test]
    fn no_compute_matrix_by_detail_and_intent() {
        let stop = operation(UserAppOperationKind::Stop);
        let start = operation(UserAppOperationKind::Start);

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(Some(&stop)),
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Missing,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopping);

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(None),
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Stopped,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopped);

        let response = merge_observation(
            "194",
            UserappStage::Dev,
            &ReadinessControl::ordinary(None),
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Missing,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::NotDeployed);

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(Some(&start)),
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Missing,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Starting);
    }

    /// 传输失败/不支持/换代分别归类；admin 不可达 + Stop 受理 → stopping。
    #[test]
    fn transport_failures_classify_without_faking_states() {
        let physical = UserAppReadinessPhysical {
            instance_id: Some("pod-uid".into()),
            address: Some("10.0.0.1:3010".into()),
            channel: UserAppReadinessChannel::Direct,
        };
        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(None),
            UserAppReadinessObservation::AdminUnreachable {
                physical: physical.clone(),
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Unknown);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::AdminUnreachable)
        );

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(None),
            UserAppReadinessObservation::UnsupportedRuntime { physical },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Unsupported);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::RuntimeUpgradeRequired)
        );

        let stop = operation(UserAppOperationKind::Stop);
        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(Some(&stop)),
            UserAppReadinessObservation::AdminUnreachable {
                physical: UserAppReadinessPhysical {
                    instance_id: None,
                    address: None,
                    channel: UserAppReadinessChannel::Exec,
                },
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopping);
    }

    #[test]
    fn stopped_builder_and_removed_container_are_not_undeployed() {
        let mut control = ReadinessControl::ordinary(None);
        let response = merge_observation(
            "194",
            UserappStage::Dev,
            &control,
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Stopped,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopped);
        control.desired_stopped = true;
        let response = merge_observation(
            "194",
            UserappStage::Dev,
            &control,
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Missing,
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopped);
        let response = merge_observation(
            "194",
            UserappStage::Dev,
            &control,
            UserAppReadinessObservation::Snapshot {
                physical: UserAppReadinessPhysical {
                    instance_id: None,
                    address: None,
                    channel: UserAppReadinessChannel::Exec,
                },
                snapshot: ready_snapshot(),
            },
        );
        assert_eq!(
            response.status,
            UserAppReadinessStatus::Ready,
            "historical Stop intent must not hide a later live owner"
        );
        assert!(response.ready);
    }

    #[tokio::test]
    async fn query_rereads_compute_stop_admitted_during_observation_and_isolates_prod() {
        use shared_types::UserAppLifecycleStore;
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
                        channel: UserAppReadinessChannel::Exec,
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
        assert!(!response.ready);
        assert_eq!(response.operation_id.as_deref(), Some("stopdev"));
        let response = service
            .get_app_readiness(UserappStage::Prod, "194")
            .await
            .unwrap();
        assert_eq!(response.status, UserAppReadinessStatus::Ready);
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
            shared_types::ComputeControlState::Pending
        );
    }
}

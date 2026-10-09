//! 就绪状态纯推导层：存储控制意图（`ReadinessControl`）与运行时观察
//! （`UserAppReadinessObservation` / dbx 探测事实）双源合并，产出
//! `/readiness` 顶层响应与两路共用的 `container` 容器状态。无 I/O。
//!
//! 容器口径单一事实源：`container_readiness` 同时服务
//! `/readiness`（经 [`ContainerObservation::from`]）与
//! `/dbx/readiness`（经 [`dbx_container_observation`]）。

use shared_types::{
    ComputeControlAction, ComputeControlState, UserAppComputeStatus, UserAppContainerOperation,
    UserAppContainerReadiness, UserAppContainerStatus, UserAppNoComputeState, UserAppOperationKind,
    UserAppOperationRecord, UserAppProxyReadiness, UserAppReadinessObservation,
    UserAppReadinessReason, UserAppReadinessResponse, UserAppReadinessStatus, UserappStage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlIntent {
    None,
    StopAccepted,
    Starting,
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadinessControl {
    pub(super) lifecycle_id: String,
    pub(super) intent: ControlIntent,
    pub(super) operation_id: Option<String>,
    pub(super) compute: UserAppComputeStatus,
}

impl ReadinessControl {
    pub(super) fn ordinary(operation: Option<&UserAppOperationRecord>) -> Self {
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
            compute: UserAppComputeStatus::default(),
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(super) fn merge_observation(
    app_id: &str,
    app_stage: UserappStage,
    control: &ReadinessControl,
    observation: UserAppReadinessObservation,
) -> UserAppReadinessResponse {
    use UserAppReadinessReason as Reason;
    use UserAppReadinessStatus as Status;
    let container = container_readiness(control, &ContainerObservation::from(&observation));
    let incomplete = matches!(observation, UserAppReadinessObservation::TimedOut);
    let base = |status: Status, reason: Option<Reason>| UserAppReadinessResponse {
        app_id: app_id.into(),
        app_stage: app_stage.as_str().into(),
        ready: false,
        status,
        container: container.clone(),
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
                    if control.compute.desired_stopped =>
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
            container: container.clone(),
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
    if container.status == UserAppContainerStatus::RecoveryRequired || incomplete {
        response.ready = false;
        if container.status == UserAppContainerStatus::RecoveryRequired {
            response.status = Status::Unknown;
        }
        response.reason_code = Some(Reason::ObserveIncomplete);
    }
    response
}

/// 容器状态推导的观察输入摘要：完整观察枚举按三分组归并，
/// `/readiness`（`UserAppReadinessObservation`）与 `/dbx/readiness`
/// （`DbxComputeFact`）两路共用同一推导，容器口径永远一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerObservation {
    /// 物理实例在（管理接口/业务快照成败不影响容器在跑的事实）。
    Running,
    NoCompute(UserAppNoComputeState),
    Unknown,
}

impl From<&UserAppReadinessObservation> for ContainerObservation {
    fn from(observation: &UserAppReadinessObservation) -> Self {
        match observation {
            UserAppReadinessObservation::Snapshot { .. }
            | UserAppReadinessObservation::AdminUnreachable { .. }
            | UserAppReadinessObservation::UnsupportedRuntime { .. } => Self::Running,
            UserAppReadinessObservation::InstanceChanged
            | UserAppReadinessObservation::TimedOut => Self::Unknown,
            UserAppReadinessObservation::NoCompute { state } => Self::NoCompute(*state),
        }
    }
}

/// dbx 探测事实 → 容器推导输入。container 只描述物理容器，与 4224
/// 探测成败解耦：定位稳定即 running（dbx 未应答反映在顶层 starting）。
pub(super) fn dbx_container_observation(
    observation: &shared_types::DbxReadinessObservation,
) -> ContainerObservation {
    use shared_types::{DbxComputeFact, DbxReadinessReason};
    match observation.compute {
        DbxComputeFact::NotRunning(state) => ContainerObservation::NoCompute(state),
        DbxComputeFact::Located
            if observation.reason_code == Some(DbxReadinessReason::InstanceChanged) =>
        {
            ContainerObservation::Unknown
        }
        DbxComputeFact::Located => ContainerObservation::Running,
        DbxComputeFact::Unobserved => ContainerObservation::Unknown,
    }
}

pub(super) fn container_readiness(
    control: &ReadinessControl,
    observation: &ContainerObservation,
) -> UserAppContainerReadiness {
    use UserAppContainerStatus as Status;
    let operation = control.compute.operation.as_ref();
    // Docker may report an intentional forced stop as exited(137)/Failed.
    // Only the current, confirmed Stop can classify that exit as stopped;
    // an unconfirmed/failed request or a running instance cannot do so.
    let confirmed_stop = control.compute.desired_stopped
        && operation.is_some_and(|record| {
            record.action == ComputeControlAction::Stop
                && record.state == ComputeControlState::Succeeded
        });
    let progress = operation.and_then(|record| match record.state {
        ComputeControlState::Pending | ComputeControlState::Running => Some(match record.action {
            ComputeControlAction::Stop => Status::Stopping,
            ComputeControlAction::Restart => Status::Restarting,
        }),
        ComputeControlState::RecoveryRequired => Some(Status::RecoveryRequired),
        ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => None,
    });
    let status = progress.unwrap_or(match observation {
        ContainerObservation::Running => Status::Running,
        ContainerObservation::Unknown => Status::Unknown,
        ContainerObservation::NoCompute(state) => match state {
            UserAppNoComputeState::Missing if control.compute.desired_stopped => Status::Stopped,
            UserAppNoComputeState::Missing => Status::Missing,
            UserAppNoComputeState::Starting => Status::Starting,
            UserAppNoComputeState::Stopping => Status::Stopping,
            UserAppNoComputeState::Stopped => Status::Stopped,
            UserAppNoComputeState::Failed if confirmed_stop => Status::Stopped,
            UserAppNoComputeState::Failed => Status::Failed,
            UserAppNoComputeState::Unknown => Status::Unknown,
        },
    });
    UserAppContainerReadiness {
        status,
        operation: operation.map(UserAppContainerOperation::from),
    }
}

/// 推导层共享测试 fixtures（服务级测试在 `mod.rs` / `dbx.rs` 也用快照）。
#[cfg(test)]
pub(super) mod fixtures {
    use shared_types::{
        UserAppBusinessReadiness, UserAppOperationKind, UserAppOperationRecord,
        UserAppOperationScope, UserAppOperationState, UserAppProxyReadiness,
        UserAppReadinessStatus,
    };

    pub(crate) fn operation(kind: UserAppOperationKind) -> UserAppOperationRecord {
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

    pub(crate) fn ready_snapshot() -> UserAppBusinessReadiness {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::{operation, ready_snapshot};
    use super::*;
    use shared_types::UserAppLifecycleStore;
    use shared_types::{UserAppOperationScope, UserAppReadinessChannel, UserAppReadinessPhysical};
    use std::sync::Arc;

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
        control.compute.desired_stopped = true;
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
    async fn container_control_result_and_business_health_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let (service, store) = crate::test_support::test_service_with_store(
            root.path(),
            Arc::new(crate::test_support::MockRuntime::default()),
        )
        .await;
        let app = store.ensure_identity("194").await.unwrap();
        let record = store
            .admit_compute_control(&shared_types::ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id,
                scope: UserAppOperationScope::Prod,
                operation_id: "restartprod".into(),
                request_id: "restartprod".into(),
                request_fingerprint: "a".repeat(64),
                action: ComputeControlAction::Restart,
                restart_image_roll: true,
            })
            .await
            .unwrap();
        let baseline = service
            .read_readiness_control("194", UserappStage::Prod)
            .await
            .unwrap();
        for (state, expected) in [
            (
                ComputeControlState::Pending,
                UserAppContainerStatus::Restarting,
            ),
            (
                ComputeControlState::Running,
                UserAppContainerStatus::Restarting,
            ),
            (
                ComputeControlState::RecoveryRequired,
                UserAppContainerStatus::RecoveryRequired,
            ),
            (
                ComputeControlState::Succeeded,
                UserAppContainerStatus::Running,
            ),
            (ComputeControlState::Failed, UserAppContainerStatus::Running),
            (
                ComputeControlState::Superseded,
                UserAppContainerStatus::Running,
            ),
        ] {
            let mut control = baseline.clone();
            let mut current = record.clone();
            current.state = state;
            current.error_code =
                (state == ComputeControlState::Failed).then(|| "ERR_BACKEND_ERROR".into());
            current.error_message =
                (state == ComputeControlState::Failed).then(|| "earlier restart failed".into());
            control.compute.operation = Some(current);
            if state.is_terminal() {
                control.intent = ControlIntent::None;
            }
            let response = merge_observation(
                "194",
                UserappStage::Prod,
                &control,
                UserAppReadinessObservation::Snapshot {
                    physical: UserAppReadinessPhysical {
                        instance_id: Some("live-pod".into()),
                        address: None,
                        channel: UserAppReadinessChannel::Direct,
                    },
                    snapshot: ready_snapshot(),
                },
            );
            assert_eq!(response.container.status, expected, "{state:?}");
            assert_eq!(response.container.operation.as_ref().unwrap().state, state);
            assert_eq!(response.ready, state.is_terminal());
            let json = serde_json::to_value(&response).unwrap();
            assert!(json["container"]["operation"].get("lease").is_none());
            assert!(json["container"]["operation"].get("checkpoint").is_none());
            if state == ComputeControlState::Failed {
                assert_eq!(
                    json["container"]["operation"]["error_message"],
                    "earlier restart failed"
                );
            }
            let mut old_response = json;
            old_response.as_object_mut().unwrap().remove("container");
            let decoded: UserAppReadinessResponse = serde_json::from_value(old_response).unwrap();
            assert_eq!(decoded.container.status, UserAppContainerStatus::Unknown);
        }
        // A successful deliberate stop may end with Docker's non-zero exit
        // status. It must not turn into a failure or hide a later live instance.
        let mut stopped = baseline.clone();
        stopped.intent = ControlIntent::None;
        stopped.compute.desired_stopped = true;
        let stop = stopped.compute.operation.as_mut().unwrap();
        stop.action = ComputeControlAction::Stop;
        stop.state = ComputeControlState::Succeeded;
        let exit = UserAppReadinessObservation::NoCompute {
            state: UserAppNoComputeState::Failed,
        };
        assert_eq!(
            container_readiness(&stopped, &ContainerObservation::from(&exit)).status,
            UserAppContainerStatus::Stopped
        );
        assert_eq!(
            container_readiness(
                &stopped,
                &ContainerObservation::from(&UserAppReadinessObservation::AdminUnreachable {
                    physical: UserAppReadinessPhysical {
                        instance_id: Some("manually-started".into()),
                        address: None,
                        channel: UserAppReadinessChannel::Direct
                    },
                })
            )
            .status,
            UserAppContainerStatus::Running
        );
        stopped.compute.operation.as_mut().unwrap().state = ComputeControlState::Failed;
        assert_eq!(
            container_readiness(&stopped, &ContainerObservation::from(&exit)).status,
            UserAppContainerStatus::Failed
        );

        // A business restart does not claim the container itself is restarting.
        let response = merge_observation(
            "194",
            UserappStage::Prod,
            &ReadinessControl::ordinary(Some(&operation(UserAppOperationKind::RestartDeployment))),
            UserAppReadinessObservation::AdminUnreachable {
                physical: UserAppReadinessPhysical {
                    instance_id: Some("live-pod".into()),
                    address: None,
                    channel: UserAppReadinessChannel::Direct,
                },
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Starting);
        assert_eq!(response.container.status, UserAppContainerStatus::Running);
        assert!(response.container.operation.is_none());
    }
}

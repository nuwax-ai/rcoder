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
    /// 仅申请/接管开发 builder 计算资源，业务版本尚未参与。
    Provisioning,
}

/// [`ControlIntent`] 归一出的等待类证据：决定「观察不到业务事实」时能否承诺
/// 一个等待态。穷尽匹配，新增意图变体必须在此显式表态，不得由 `matches!`
/// 隐式承接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitEvidence {
    /// 不承诺等待：无在途操作或停止已受理，观察失败如实透出为故障。
    NoWait,
    /// 业务编排正在启动或重启，可以据控制意图宣称 `starting`。
    BusinessBoot,
    /// 只有平台在准备计算资源，业务快照尚未产生，宣称 `provisioning`。
    ComputeProvisioning,
}

impl ControlIntent {
    fn wait(self) -> WaitEvidence {
        match self {
            Self::None | Self::StopAccepted => WaitEvidence::NoWait,
            Self::Starting | Self::Restarting => WaitEvidence::BusinessBoot,
            Self::Provisioning => WaitEvidence::ComputeProvisioning,
        }
    }
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
            None => ControlIntent::None,
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
                | UserAppOperationKind::Update
                | UserAppOperationKind::HotDeploy,
            ) => ControlIntent::Starting,
            // EnsureBuilder/AdoptBuilder 只准备或接管开发 builder 的计算资源，
            // 不启动任何业务版本：这个窗口的等待证据是资源准备，不能占用
            // 业务启动语义。
            Some(UserAppOperationKind::EnsureBuilder | UserAppOperationKind::AdoptBuilder) => {
                ControlIntent::Provisioning
            }
            // 元数据、存储、数据库凭据与实例收养类操作不改变业务态：观察失败
            // 时如实透出故障，不编造等待态。
            Some(
                UserAppOperationKind::SetRecyclePolicy
                | UserAppOperationKind::AdoptApplication
                | UserAppOperationKind::PurgeResources
                | UserAppOperationKind::DestroyDevStorage
                | UserAppOperationKind::DestroyProdStorage
                | UserAppOperationKind::ClearDevStorage
                | UserAppOperationKind::ClearProdStorage
                | UserAppOperationKind::ResetDevDatabasePassword
                | UserAppOperationKind::ResetProdDatabasePassword
                | UserAppOperationKind::PrepareProdDatabase
                | UserAppOperationKind::DeleteApplication,
            ) => ControlIntent::None,
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
            confirmation: None,
            ready: false,
            status,
            reason_code: reason,
            error_origin_contract: None,
        },
        services: Vec::new(),
    };
    let wait = control.intent.wait();
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
        UserAppReadinessObservation::AdminUnreachable { .. } => match wait {
            WaitEvidence::NoWait => base(Status::Unknown, Some(Reason::AdminUnreachable)),
            WaitEvidence::BusinessBoot => base(Status::Starting, Some(Reason::ServiceStarting)),
            // builder 进程还在拉起时管理端口本就不应答，这是等待而非故障。
            // 此格刻意不再透出 ADMIN_UNREACHABLE：否则一次资源申请会输出
            // provisioning → unknown → not_deployed，比现状更难解释。无在途
            // 资源操作时（NoWait 那格）仍如实报 ADMIN_UNREACHABLE。
            WaitEvidence::ComputeProvisioning => {
                base(Status::Provisioning, Some(Reason::ComputeProvisioning))
            }
        },
        UserAppReadinessObservation::NoCompute { state } => {
            // 每个格子显式表态：等待分类不得吞掉物理事实，物理事实也不能
            // 反过来吞掉等待分类。
            let (status, reason) = match (state, wait, control.compute.desired_stopped) {
                // 计算控制头的停止意图先于等待分类：故意停止后容器可能以
                // 非零状态收束，那仍是 stopped，不能被等待态洗成启动中。
                (
                    UserAppNoComputeState::Missing
                    | UserAppNoComputeState::Stopped
                    | UserAppNoComputeState::Failed,
                    WaitEvidence::NoWait
                    | WaitEvidence::BusinessBoot
                    | WaitEvidence::ComputeProvisioning,
                    true,
                ) => (Status::Stopped, None),
                // 资源申请等待覆盖 Missing/Stopped/Starting：整个窗口只出一个
                // provisioning，业务快照一旦出现即由 Snapshot 分支交还 app-cli。
                (
                    UserAppNoComputeState::Missing
                    | UserAppNoComputeState::Stopped
                    | UserAppNoComputeState::Starting,
                    WaitEvidence::ComputeProvisioning,
                    false | true,
                ) => (Status::Provisioning, Some(Reason::ComputeProvisioning)),
                // 容器正在拉起是物理事实，无在途操作也会成立（流量唤醒、
                // Clear/Reset 之后自愈），因此保持 starting，不由意图决定。
                (
                    UserAppNoComputeState::Starting,
                    WaitEvidence::NoWait | WaitEvidence::BusinessBoot,
                    false | true,
                ) => (Status::Starting, Some(Reason::ServiceStarting)),
                (
                    UserAppNoComputeState::Missing | UserAppNoComputeState::Stopped,
                    WaitEvidence::BusinessBoot,
                    false,
                ) => (Status::Starting, Some(Reason::ServiceStarting)),
                (UserAppNoComputeState::Missing, WaitEvidence::NoWait, false) => {
                    (Status::NotDeployed, None)
                }
                (UserAppNoComputeState::Stopped, WaitEvidence::NoWait, false) => {
                    (Status::Stopped, None)
                }
                // 编排失败是确定故障，任何等待分类都不覆盖它。
                (
                    UserAppNoComputeState::Failed,
                    WaitEvidence::NoWait
                    | WaitEvidence::BusinessBoot
                    | WaitEvidence::ComputeProvisioning,
                    false,
                ) => (Status::Failed, Some(Reason::OrchestrationFailed)),
                (
                    UserAppNoComputeState::Stopping,
                    WaitEvidence::NoWait
                    | WaitEvidence::BusinessBoot
                    | WaitEvidence::ComputeProvisioning,
                    false | true,
                ) => (Status::Stopping, Some(Reason::StopAccepted)),
                (
                    UserAppNoComputeState::Unknown,
                    WaitEvidence::NoWait
                    | WaitEvidence::BusinessBoot
                    | WaitEvidence::ComputeProvisioning,
                    false | true,
                ) => (Status::Unknown, Some(Reason::ObserveIncomplete)),
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
    // `Provisioning` 明确没有后置覆盖：它的权威来源永远是 app-cli 快照，
    // 一旦有快照就原样透出，不因为操作仍占着槽位而继续宣称等待。
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
                confirmation: None,
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
                snapshot: Box::new(snapshot),
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
                snapshot: Box::new(ready_snapshot()),
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
                snapshot: Box::new(ready_snapshot()),
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
                builder_startup_mode: Default::default(),
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
                    snapshot: Box::new(ready_snapshot()),
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

    fn admin_unreachable() -> UserAppReadinessObservation {
        UserAppReadinessObservation::AdminUnreachable {
            physical: UserAppReadinessPhysical {
                instance_id: Some("pod-uid".into()),
                address: Some("10.0.0.1:3010".into()),
                channel: UserAppReadinessChannel::Exec,
            },
        }
    }

    /// 资源申请窗口不得冒充业务启动：`EnsureBuilder` 在途时，「问不到业务」
    /// 的等待证据是 `provisioning`（平台正在准备计算资源），不是 `starting`
    /// （应用业务正在启动）。前端据此区分「还没开始启动」与「启动中」。
    #[test]
    fn builder_provisioning_is_not_reported_as_business_start() {
        let control =
            ReadinessControl::ordinary(Some(&operation(UserAppOperationKind::EnsureBuilder)));
        for observation in [
            admin_unreachable(),
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Missing,
            },
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Starting,
            },
            UserAppReadinessObservation::NoCompute {
                state: UserAppNoComputeState::Stopped,
            },
        ] {
            let response = merge_observation("194", UserappStage::Dev, &control, observation);
            assert_eq!(
                response.status,
                UserAppReadinessStatus::Provisioning,
                "builder wait must not be reported as business boot"
            );
            assert_eq!(
                response.reason_code,
                Some(UserAppReadinessReason::ComputeProvisioning)
            );
            assert!(!response.ready);
            assert_eq!(response.operation_id.as_deref(), Some("op-1"));
            assert_eq!(
                response.proxy.status,
                UserAppReadinessStatus::Provisioning,
                "合成分支下 proxy 与顶层同值"
            );
            assert_eq!(response.proxy.reason_code, response.reason_code);
        }
    }

    /// 业务编排类操作保持 `starting`：本次拆分只把纯资源操作移出该档位，
    /// 不扩大等待态的适用范围。
    #[test]
    fn business_operations_keep_starting() {
        for kind in [
            UserAppOperationKind::Create,
            UserAppOperationKind::StartDeployment,
            UserAppOperationKind::Start,
            UserAppOperationKind::Update,
            UserAppOperationKind::HotDeploy,
        ] {
            let control = ReadinessControl::ordinary(Some(&operation(kind)));
            for observation in [
                admin_unreachable(),
                UserAppReadinessObservation::NoCompute {
                    state: UserAppNoComputeState::Missing,
                },
                UserAppReadinessObservation::NoCompute {
                    state: UserAppNoComputeState::Stopped,
                },
            ] {
                let response = merge_observation("194", UserappStage::Prod, &control, observation);
                assert_eq!(
                    response.status,
                    UserAppReadinessStatus::Starting,
                    "{kind:?}"
                );
                assert_eq!(
                    response.reason_code,
                    Some(UserAppReadinessReason::ServiceStarting),
                    "{kind:?}"
                );
                assert_ne!(
                    response.status,
                    UserAppReadinessStatus::Provisioning,
                    "{kind:?} is a business boot, not compute provisioning"
                );
            }
        }
    }

    fn builder_control() -> ReadinessControl {
        ReadinessControl::ordinary(Some(&operation(UserAppOperationKind::EnsureBuilder)))
    }

    fn no_compute(state: UserAppNoComputeState) -> UserAppReadinessObservation {
        UserAppReadinessObservation::NoCompute { state }
    }

    fn snapshot() -> UserAppReadinessObservation {
        UserAppReadinessObservation::Snapshot {
            physical: UserAppReadinessPhysical {
                instance_id: Some("pod-uid".into()),
                address: None,
                channel: UserAppReadinessChannel::Exec,
            },
            snapshot: Box::new(ready_snapshot()),
        }
    }

    /// 资源申请等待只替换「观察不到业务事实」的档位：确定故障、停止中、
    /// 观察不完整与旧运行时如实透出，不能被洗成 provisioning。
    #[test]
    fn provisioning_keeps_failure_and_stop_evidence() {
        let physical = UserAppReadinessPhysical {
            instance_id: Some("pod-uid".into()),
            address: Some("127.0.0.1:3010".into()),
            channel: UserAppReadinessChannel::Exec,
        };
        for (observation, status, reason) in [
            (
                no_compute(UserAppNoComputeState::Failed),
                UserAppReadinessStatus::Failed,
                Some(UserAppReadinessReason::OrchestrationFailed),
            ),
            (
                no_compute(UserAppNoComputeState::Stopping),
                UserAppReadinessStatus::Stopping,
                Some(UserAppReadinessReason::StopAccepted),
            ),
            (
                no_compute(UserAppNoComputeState::Unknown),
                UserAppReadinessStatus::Unknown,
                Some(UserAppReadinessReason::ObserveIncomplete),
            ),
            (
                UserAppReadinessObservation::UnsupportedRuntime {
                    physical: physical.clone(),
                },
                UserAppReadinessStatus::Unsupported,
                Some(UserAppReadinessReason::RuntimeUpgradeRequired),
            ),
            (
                UserAppReadinessObservation::InstanceChanged,
                UserAppReadinessStatus::Unknown,
                Some(UserAppReadinessReason::InstanceChanged),
            ),
            (
                UserAppReadinessObservation::TimedOut,
                UserAppReadinessStatus::Unknown,
                Some(UserAppReadinessReason::ObserveIncomplete),
            ),
        ] {
            let response =
                merge_observation("194", UserappStage::Dev, &builder_control(), observation);
            assert_eq!(
                response.status, status,
                "{status:?} must survive the wait intent"
            );
            assert_ne!(response.status, UserAppReadinessStatus::Provisioning);
            assert_eq!(response.reason_code, reason);
        }
    }

    /// builder 收养（AdoptBuilder）是资源接管，不是业务启动，也不是故障：
    /// 观察失败/实例缺失期间同样是 `provisioning`。
    #[test]
    fn builder_adoption_is_not_a_fault() {
        let control =
            ReadinessControl::ordinary(Some(&operation(UserAppOperationKind::AdoptBuilder)));
        for observation in [
            admin_unreachable(),
            no_compute(UserAppNoComputeState::Missing),
        ] {
            let response = merge_observation("194", UserappStage::Dev, &control, observation);
            assert_eq!(response.status, UserAppReadinessStatus::Provisioning);
            assert_eq!(
                response.reason_code,
                Some(UserAppReadinessReason::ComputeProvisioning)
            );
        }
    }

    /// 计算控制头的停止意图先于等待分类：故意停止后的缺失/停止/非零退出
    /// 都停在 `stopped`，不得被 provisioning 覆盖。
    #[test]
    fn desired_stopped_outranks_provisioning_wait() {
        let mut control = builder_control();
        control.compute.desired_stopped = true;
        for state in [
            UserAppNoComputeState::Missing,
            UserAppNoComputeState::Stopped,
            UserAppNoComputeState::Failed,
        ] {
            let response = merge_observation("194", UserappStage::Dev, &control, no_compute(state));
            assert_eq!(
                response.status,
                UserAppReadinessStatus::Stopped,
                "{state:?}"
            );
            assert_ne!(
                response.status,
                UserAppReadinessStatus::Provisioning,
                "{state:?} under an accepted stop must not claim provisioning"
            );
        }
    }

    /// app-cli 快照是业务态权威：窗口一结束就原样透出，`Provisioning` 没有
    /// 任何后置覆盖。
    #[test]
    fn snapshot_ends_the_provisioning_window() {
        let response = merge_observation("194", UserappStage::Dev, &builder_control(), snapshot());
        assert!(response.ready);
        assert_eq!(response.status, UserAppReadinessStatus::Ready);
        assert_eq!(response.observation_revision, 5);
        assert_eq!(response.runtime_instance_id.as_deref(), Some("instance-1"));
        assert_eq!(response.operation_id.as_deref(), Some("op-1"));
        assert_eq!(
            response.runtime_operation_id.as_deref(),
            Some("runtime-op-1")
        );
    }

    /// 容器正在拉起是无在途操作也会成立的物理事实（流量唤醒、Clear/Reset
    /// 之后自愈），因此保持 `starting`，不改成意图驱动。
    #[test]
    fn no_compute_starting_without_operation_stays_starting() {
        let response = merge_observation(
            "194",
            UserappStage::Dev,
            &ReadinessControl::ordinary(None),
            no_compute(UserAppNoComputeState::Starting),
        );
        assert_eq!(response.status, UserAppReadinessStatus::Starting);
        assert_eq!(
            response.reason_code,
            Some(UserAppReadinessReason::ServiceStarting)
        );
    }

    /// kind→意图映射的单一事实源逐格钉住：新增操作 kind 必须在此表态，
    /// 不能由默认分支落到 `starting`（那会把资源等待重新混进业务启动）。
    #[test]
    fn operation_kind_intent_table_is_explicit() {
        const EXPECTED: [(UserAppOperationKind, ControlIntent); 24] = [
            (UserAppOperationKind::Stop, ControlIntent::StopAccepted),
            (
                UserAppOperationKind::StopBuilder,
                ControlIntent::StopAccepted,
            ),
            (
                UserAppOperationKind::DeleteCompute,
                ControlIntent::StopAccepted,
            ),
            (UserAppOperationKind::Restart, ControlIntent::Restarting),
            (
                UserAppOperationKind::RestartDeployment,
                ControlIntent::Restarting,
            ),
            (
                UserAppOperationKind::RestartBuilder,
                ControlIntent::Restarting,
            ),
            (UserAppOperationKind::Create, ControlIntent::Starting),
            (
                UserAppOperationKind::StartDeployment,
                ControlIntent::Starting,
            ),
            (UserAppOperationKind::Start, ControlIntent::Starting),
            (UserAppOperationKind::Update, ControlIntent::Starting),
            (UserAppOperationKind::HotDeploy, ControlIntent::Starting),
            (
                UserAppOperationKind::EnsureBuilder,
                ControlIntent::Provisioning,
            ),
            (
                UserAppOperationKind::AdoptBuilder,
                ControlIntent::Provisioning,
            ),
            (UserAppOperationKind::SetRecyclePolicy, ControlIntent::None),
            (UserAppOperationKind::AdoptApplication, ControlIntent::None),
            (UserAppOperationKind::PurgeResources, ControlIntent::None),
            (UserAppOperationKind::DestroyDevStorage, ControlIntent::None),
            (
                UserAppOperationKind::DestroyProdStorage,
                ControlIntent::None,
            ),
            (UserAppOperationKind::ClearDevStorage, ControlIntent::None),
            (UserAppOperationKind::ClearProdStorage, ControlIntent::None),
            (
                UserAppOperationKind::ResetDevDatabasePassword,
                ControlIntent::None,
            ),
            (
                UserAppOperationKind::ResetProdDatabasePassword,
                ControlIntent::None,
            ),
            (
                UserAppOperationKind::PrepareProdDatabase,
                ControlIntent::None,
            ),
            (UserAppOperationKind::DeleteApplication, ControlIntent::None),
        ];
        for (kind, intent) in EXPECTED {
            let control = ReadinessControl::ordinary(Some(&operation(kind)));
            assert_eq!(control.intent, intent, "{kind:?}");
            assert_eq!(control.operation_id.as_deref(), Some("op-1"), "{kind:?}");
        }
        assert_eq!(ReadinessControl::ordinary(None).intent, ControlIntent::None);
        assert_eq!(
            ReadinessControl::ordinary(None).operation_id,
            None,
            "无在途操作不得带出 operation_id"
        );
        assert_eq!(
            EXPECTED
                .iter()
                .filter(|(_, intent)| *intent == ControlIntent::Provisioning)
                .map(|(kind, _)| *kind)
                .collect::<Vec<_>>(),
            vec![
                UserAppOperationKind::EnsureBuilder,
                UserAppOperationKind::AdoptBuilder
            ],
            "资源申请等待态目前只由 builder 资源操作产出（申请 + 收养）"
        );
    }
}

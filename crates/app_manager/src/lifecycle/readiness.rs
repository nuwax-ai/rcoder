//! 业务就绪查询只读合并平台控制意图和实际实例的 app-cli 快照。
//! 不申请操作锁，不刷新闲置计时，也不推进恢复/启停。观察不是准入依据。

use std::sync::Arc;
use std::time::Duration;

use shared_types::{
    ComputeControlAction, ComputeControlState, UserAppComputeStatus, UserAppContainerOperation,
    UserAppContainerReadiness, UserAppContainerStatus, UserAppLifecycleState,
    UserAppNoComputeState, UserAppOperationKind, UserAppOperationRecord, UserAppOperationScope,
    UserAppProxyReadiness, UserAppReadinessObservation, UserAppReadinessReason,
    UserAppReadinessResponse, UserAppReadinessStatus, UserappStage,
};
use tokio::time::Instant;

use crate::error::AppOperationError;
use crate::models::AppResult;
use crate::service::AppService;
use crate::utils::validate_app_id;

pub(crate) const READINESS_QUERY_BUDGET: Duration = Duration::from_secs(8);
const DBX_READINESS_QUERY_BUDGET: Duration = Duration::from_secs(3);

/// Poll the authoritative read inside the same deadline as the runtime query.
/// Keeping the read as a future also permits a stalled-store regression without
/// replacing the production lifecycle store or changing its mutation contract.
async fn query_dbx_with_budget(
    app_id: &str,
    stage: UserappStage,
    budget: Duration,
    control: impl Future<Output = AppResult<ReadinessControl>>,
    prober: Option<Arc<dyn shared_types::DbxReadinessProber>>,
) -> AppResult<shared_types::DbxReadinessResponse> {
    use shared_types::{DbxReadinessObservation, DbxReadinessReason, DbxReadinessStatus};
    let deadline = Instant::now() + budget;
    let observe = async {
        // 存储控制读取复用 /readiness 的 read_readiness_control（含 404 语义）：
        // 在途 Stop/Restart 回执与 desired_stopped 参与 container 推导。
        let control = control.await?;
        let prober = prober.ok_or_else(|| {
            AppOperationError::Backend("dbx prober is not wired (host assembly missing)".into())
        })?;
        let observation = match prober
            .probe(
                app_id,
                stage,
                deadline.saturating_duration_since(Instant::now()),
            )
            .await
        {
            Ok(observation) => observation,
            Err(error) => {
                tracing::warn!(app_id, %error, "DBX readiness observation failed");
                DbxReadinessObservation::new(
                    DbxReadinessStatus::Unknown,
                    Some(DbxReadinessReason::ObservationFailed),
                )
                .with_message(error)
            }
        };
        let container = container_readiness(&control, &dbx_container_observation(&observation));
        Ok(observation.into_response(container))
    };
    tokio::time::timeout_at(deadline, observe)
        .await
        .unwrap_or_else(|_| {
            // 预算耗尽时存储读取可能未完成（无控制事实）：容器按 unknown 收场，
            // 与 /readiness 的 TimedOut 语义一致。
            Ok(DbxReadinessObservation::new(
                DbxReadinessStatus::Unknown,
                Some(DbxReadinessReason::ObserveIncomplete),
            )
            .into_response(UserAppContainerReadiness::default()))
        })
}

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
        query_dbx_with_budget(
            app_id,
            app_stage,
            DBX_READINESS_QUERY_BUDGET,
            self.read_readiness_control(app_id, app_stage),
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
    compute: UserAppComputeStatus,
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
            compute: UserAppComputeStatus::default(),
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
enum ContainerObservation {
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
fn dbx_container_observation(
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

fn container_readiness(
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::UserAppLifecycleStore;
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

    /// dbx readiness：prober 三态映射 + 探测失败降级 unknown（带原因码）+
    /// 权威记录缺失 404 + 全程只读（runtime 零写调用）。
    #[tokio::test]
    async fn dbx_readiness_maps_probe_outcomes_and_missing_app() {
        use std::sync::atomic::{AtomicU8, Ordering};

        struct Probe(AtomicU8);
        #[async_trait::async_trait]
        impl shared_types::DbxReadinessProber for Probe {
            async fn probe(
                &self,
                _: &str,
                _: UserappStage,
                _: Duration,
            ) -> Result<shared_types::DbxReadinessObservation, String> {
                match self.0.load(Ordering::SeqCst) {
                    0 => Ok(shared_types::DbxReadinessObservation::new(
                        shared_types::DbxReadinessStatus::Ready,
                        None,
                    )
                    .with_compute_fact(shared_types::DbxComputeFact::Located)),
                    1 => Ok(shared_types::DbxReadinessObservation::new(
                        shared_types::DbxReadinessStatus::Stopped,
                        Some(shared_types::DbxReadinessReason::ComputeStopped),
                    )
                    .with_compute_fact(
                        shared_types::DbxComputeFact::NotRunning(UserAppNoComputeState::Stopped),
                    )),
                    _ => Err("probe transport blew up".into()),
                }
            }
        }

        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        store.ensure_identity("dbx1").await.unwrap();

        // 权威记录不存在 → NotFound（错误信封 404），不触发探测。
        assert!(
            service
                .get_app_dbx_readiness(UserappStage::Prod, "ghost")
                .await
                .is_err()
        );

        service
            .set_dbx_prober(Arc::new(Probe(AtomicU8::new(0))))
            .unwrap();
        let response = service
            .get_app_dbx_readiness(UserappStage::Prod, "dbx1")
            .await
            .unwrap();
        assert!(response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Ready);
        assert!(response.reason_code.is_none());
        // 实例定位稳定 → container 与 dbx 探测同轮一致为 running。
        assert_eq!(response.container.status, UserAppContainerStatus::Running);

        service
            .set_dbx_prober(Arc::new(Probe(AtomicU8::new(1))))
            .unwrap();
        let response = service
            .get_app_dbx_readiness(UserappStage::Dev, "dbx1")
            .await
            .unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Stopped);
        assert_eq!(response.container.status, UserAppContainerStatus::Stopped);

        service
            .set_dbx_prober(Arc::new(Probe(AtomicU8::new(2))))
            .unwrap();
        let response = service
            .get_app_dbx_readiness(UserappStage::Prod, "dbx1")
            .await
            .unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
        assert_eq!(
            response.reason_code,
            Some(shared_types::DbxReadinessReason::ObservationFailed)
        );
        assert_eq!(response.message.as_deref(), Some("probe transport blew up"));
        // 探测系统失败无计算事实 → container unknown，不伪装 stopped。
        assert_eq!(response.container.status, UserAppContainerStatus::Unknown);

        // 只读性：查询不产生任何 runtime 写调用（不唤醒、不建容器）。
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    }

    /// dbx container 状态矩阵：存储控制回执优先于探测器观察——在途 Stop
    /// 期间实例已消失不能伪装 stopped（用户点启动会撞在途操作）；
    /// recovery_required 透出操作错误；Located 与 4224 探测成败解耦。
    #[tokio::test]
    async fn dbx_container_merges_control_receipt_with_compute_fact() {
        use shared_types::{
            ComputeControlAction, ComputeControlState, DbxComputeFact, DbxReadinessObservation,
            DbxReadinessReason, DbxReadinessStatus, UserAppComputeStatus,
        };

        fn compute_record(
            action: ComputeControlAction,
            state: ComputeControlState,
        ) -> ReadinessControl {
            let mut record = shared_types::ComputeControlRecord {
                created_at: chrono::Utc::now(),
                app_id: "dbx1".into(),
                lifecycle_id: "life".into(),
                request_id: "req".into(),
                request_fingerprint: "a".repeat(64),
                scope: UserAppOperationScope::Prod,
                operation_id: format!("op-{state:?}"),
                generation: 1,
                revision: 1,
                action,
                state,
                executor_id: None,
                stage: "executing".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
                lease: None,
                interrupted_operations: Vec::new(),
            };
            if state == ComputeControlState::RecoveryRequired {
                record.error_code = Some("ERR_BACKEND_ERROR".into());
                record.error_message = Some("stop cleanup unconfirmed".into());
            }
            let mut control = ReadinessControl::ordinary(None);
            control.compute = UserAppComputeStatus {
                desired_stopped: false,
                generation: 1,
                revision: 1,
                operation: Some(record),
            };
            control
        }

        fn stopped_observation() -> DbxReadinessObservation {
            DbxReadinessObservation::new(
                DbxReadinessStatus::Stopped,
                Some(DbxReadinessReason::ComputeStopped),
            )
            .with_compute_fact(DbxComputeFact::NotRunning(UserAppNoComputeState::Stopped))
        }

        struct Fixed(DbxReadinessObservation);
        #[async_trait::async_trait]
        impl shared_types::DbxReadinessProber for Fixed {
            async fn probe(
                &self,
                _: &str,
                _: UserappStage,
                _: Duration,
            ) -> Result<DbxReadinessObservation, String> {
                Ok(self.0.clone())
            }
        }

        let query = |control: ReadinessControl, observation: DbxReadinessObservation| async move {
            query_dbx_with_budget(
                "dbx1",
                UserappStage::Prod,
                Duration::from_secs(2),
                async move { Ok::<_, AppOperationError>(control) },
                Some(Arc::new(Fixed(observation))),
            )
            .await
            .unwrap()
        };

        // 在途 Stop（控制回执优先）：探测器已看不到实例，容器仍是 stopping。
        let response = query(
            compute_record(ComputeControlAction::Stop, ComputeControlState::Running),
            stopped_observation(),
        )
        .await;
        assert_eq!(response.status, DbxReadinessStatus::Stopped);
        assert_eq!(response.container.status, UserAppContainerStatus::Stopping);
        let receipt = response.container.operation.expect("receipt present");
        assert_eq!(receipt.operation_id, "op-Running");
        assert_eq!(receipt.state, ComputeControlState::Running);

        // recovery_required：容器状态 + 操作错误回执透出（前端可提示而非无限轮询）。
        let response = query(
            compute_record(
                ComputeControlAction::Stop,
                ComputeControlState::RecoveryRequired,
            ),
            stopped_observation(),
        )
        .await;
        assert_eq!(
            response.container.status,
            UserAppContainerStatus::RecoveryRequired
        );
        let receipt = response.container.operation.expect("receipt present");
        assert_eq!(receipt.error_code.as_deref(), Some("ERR_BACKEND_ERROR"));
        assert_eq!(
            receipt.error_message.as_deref(),
            Some("stop cleanup unconfirmed")
        );

        // 实例在、4224 未应答：容器 running + dbx 顶层 starting（事故形态的精准区分）。
        let response = query(
            ReadinessControl::ordinary(None),
            DbxReadinessObservation::new(
                DbxReadinessStatus::Starting,
                Some(DbxReadinessReason::DbxUnreachable),
            )
            .with_compute_fact(DbxComputeFact::Located),
        )
        .await;
        assert_eq!(response.status, DbxReadinessStatus::Starting);
        assert_eq!(response.container.status, UserAppContainerStatus::Running);
        assert!(response.container.operation.is_none());

        // 停止完成 + 实例消失：容器 stopped（前端给启动引导而非检测中）。
        let mut control = ReadinessControl::ordinary(None);
        control.compute = UserAppComputeStatus {
            desired_stopped: true,
            generation: 1,
            revision: 1,
            operation: None,
        };
        let response = query(
            control,
            DbxReadinessObservation::new(
                DbxReadinessStatus::Stopped,
                Some(DbxReadinessReason::ComputeMissing),
            )
            .with_compute_fact(DbxComputeFact::NotRunning(UserAppNoComputeState::Missing)),
        )
        .await;
        assert_eq!(response.container.status, UserAppContainerStatus::Stopped);

        // 定位稳定但换代证据（InstanceChanged）：容器 unknown，不沿用过期定位。
        let response = query(
            ReadinessControl::ordinary(None),
            DbxReadinessObservation::new(
                DbxReadinessStatus::Unknown,
                Some(DbxReadinessReason::InstanceChanged),
            )
            .with_compute_fact(DbxComputeFact::Located),
        )
        .await;
        assert_eq!(response.container.status, UserAppContainerStatus::Unknown);
    }

    #[tokio::test]
    async fn dbx_total_deadline_covers_a_stalled_store_before_probing() {
        struct MustNotProbe;
        #[async_trait::async_trait]
        impl shared_types::DbxReadinessProber for MustNotProbe {
            async fn probe(
                &self,
                _: &str,
                _: UserappStage,
                _: Duration,
            ) -> Result<shared_types::DbxReadinessObservation, String> {
                panic!("A stalled authoritative read must not reach the runtime");
            }
        }
        let started = Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            query_dbx_with_budget(
                "dbx1",
                UserappStage::Prod,
                Duration::from_millis(20),
                std::future::pending(),
                Some(Arc::new(MustNotProbe)),
            ),
        )
        .await
        .expect("the whole query must be bounded")
        .unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
        assert_eq!(
            response.reason_code,
            Some(shared_types::DbxReadinessReason::ObserveIncomplete)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

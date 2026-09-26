//! UserApp 业务就绪查询：`GET /api/v1/userapp/{app_id}/{app_stage}/readiness`。
//!
//! 只读合并链：权威 lifecycle 记录（存在性/删除态）→ scope 在途操作
//! （Stop 受理/启动中）→ 宿主注入的 [`UserAppReadinessReader`] 物理观察
//! （app-cli 业务快照）→ 统一 [`UserAppReadinessResponse`]。
//!
//! 只读纪律（Spec §4）：不复用 `log_api_base`/`app_files_base`/`ensure_running`
//! 等写路径；不申请业务锁（busy/RecoveryRequired 不阻塞查询）；不刷新闲置
//! 计时；Stop/Restart 照常运行。观察结果只描述一次时间窗口，不作为准入或
//! 恢复依据。

use std::sync::Arc;

use shared_types::{
    UserAppOperationKind, UserAppOperationRecord, UserAppOperationScope, UserAppProxyReadiness,
    UserAppReadinessObservation, UserAppReadinessReason, UserAppReadinessResponse,
    UserAppReadinessStatus, UserappStage,
};

use crate::error::AppOperationError;
use crate::models::AppResult;
use crate::service::AppService;
use crate::utils::validate_app_id;

/// RCoder 侧总查询预算（含物理定位、app-cli 观察与换代复核；Plan §4.5）。
pub(crate) const READINESS_QUERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

impl AppService {
    /// 注入业务就绪只读观察器（rcoder-engine 装配；幂等覆盖仅装配期使用）。
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

    /// 业务就绪查询（Spec §5：成功观察恒 HTTP 200 + `data.ready` 才是业务可用）。
    pub async fn get_app_readiness(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<UserAppReadinessResponse> {
        validate_app_id(app_id)?;
        // 权威存在性：记录不存在或已删除 → 404（不把容器暂缺误报成应用不存在）。
        let record = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| AppOperationError::NotFound(format!("app {app_id} not found")))?;
        if record.state == shared_types::UserAppLifecycleState::Deleted {
            return Err(AppOperationError::NotFound(format!(
                "app {app_id} has been deleted"
            )));
        }

        // scope 在途操作（只读；无在途时为空——不因 busy 拒绝查询）。
        let scope = match app_stage {
            UserappStage::Dev => UserAppOperationScope::Dev,
            UserappStage::Prod => UserAppOperationScope::Prod,
        };
        let scope_operation = self
            .scope_active_operation(app_id, &record.lifecycle_id, scope)
            .await?;

        let Some(reader) = self.readiness_reader() else {
            return Err(AppOperationError::Backend(
                "readiness reader is not wired (host assembly missing)".into(),
            ));
        };
        let observation = reader
            .observe(app_id, app_stage, READINESS_QUERY_BUDGET)
            .await
            .map_err(|message| {
                AppOperationError::Backend(format!(
                    "readiness observation system failure (app {app_id}): {message}"
                ))
            })?;

        Ok(merge_observation(
            app_id,
            app_stage,
            scope_operation.as_ref(),
            observation,
        ))
    }

    /// 读取 scope 槽位当前操作记录（存储缺失视为无在途，不阻塞观察）。
    async fn scope_active_operation(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        scope: UserAppOperationScope,
    ) -> AppResult<Option<UserAppOperationRecord>> {
        let Some(operation_id) = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .filter(|record| record.lifecycle_id == lifecycle_id)
            .and_then(|record| record.active_operations.slot(scope).cloned())
        else {
            return Ok(None);
        };
        let operation = self
            .metadata
            .store
            .get_operation(app_id, &operation_id)
            .await?;
        // 记录缺失/换代：无在途证据，观察按物理事实回答。
        Ok(operation
            .filter(|record| record.lifecycle_id == lifecycle_id && !record.state.is_terminal()))
    }
}

/// 平台侧控制意图快照（合并用；从 scope 操作派生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlIntent {
    /// 无在途操作
    None,
    /// Stop 类意图已受理尚未收束
    StopAccepted,
    /// 启动/更新类操作执行中
    Starting,
}

pub(crate) fn control_intent(operation: Option<&UserAppOperationRecord>) -> ControlIntent {
    let Some(operation) = operation else {
        return ControlIntent::None;
    };
    match operation.kind {
        UserAppOperationKind::Stop
        | UserAppOperationKind::StopBuilder
        | UserAppOperationKind::DeleteCompute => ControlIntent::StopAccepted,
        UserAppOperationKind::Create
        | UserAppOperationKind::StartDeployment
        | UserAppOperationKind::RestartDeployment
        | UserAppOperationKind::Start
        | UserAppOperationKind::Restart
        | UserAppOperationKind::EnsureBuilder
        | UserAppOperationKind::Update
        | UserAppOperationKind::HotDeploy => ControlIntent::Starting,
        // 存储/密码/清理类操作不构成目标环境启停意图；观察照常。
        _ => ControlIntent::None,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 观察结果 + 控制意图 → 最终响应（纯函数，测试覆盖合并边界）。
pub(crate) fn merge_observation(
    app_id: &str,
    app_stage: UserappStage,
    scope_operation: Option<&UserAppOperationRecord>,
    observation: UserAppReadinessObservation,
) -> UserAppReadinessResponse {
    let intent = control_intent(scope_operation);
    let operation_id = scope_operation.map(|operation| operation.operation_id.clone());
    let base_unknown = |status: UserAppReadinessStatus, reason: Option<UserAppReadinessReason>| {
        UserAppReadinessResponse {
            app_id: app_id.to_string(),
            app_stage: app_stage.as_str().to_string(),
            ready: false,
            status,
            reason_code: reason,
            checked_at: now_rfc3339(),
            runtime_instance_id: None,
            serving_release_id: None,
            target_release_id: None,
            operation_id: operation_id.clone(),
            runtime_operation_id: None,
            observation_revision: 0,
            proxy: UserAppProxyReadiness {
                ready: false,
                status,
                reason_code: reason,
                error_origin_contract: None,
            },
            services: Vec::new(),
        }
    };

    match observation {
        UserAppReadinessObservation::InstanceChanged => base_unknown(
            UserAppReadinessStatus::Unknown,
            Some(UserAppReadinessReason::InstanceChanged),
        ),
        UserAppReadinessObservation::UnsupportedRuntime { physical } => {
            // 旧运行时不支持新接口：明确 unsupported，不影响用户直接访问。
            let mut response = base_unknown(
                UserAppReadinessStatus::Unsupported,
                Some(UserAppReadinessReason::RuntimeUpgradeRequired),
            );
            response.runtime_instance_id = physical.instance_id;
            response
        }
        UserAppReadinessObservation::AdminUnreachable { physical } => {
            // 计算资源在运行但管理面不可达：unknown（有停止事实时以停止为准）。
            if intent == ControlIntent::StopAccepted {
                base_unknown(
                    UserAppReadinessStatus::Stopping,
                    Some(UserAppReadinessReason::StopAccepted),
                )
            } else {
                let mut response = base_unknown(
                    UserAppReadinessStatus::Unknown,
                    Some(UserAppReadinessReason::AdminUnreachable),
                );
                response.runtime_instance_id = physical.instance_id;
                response
            }
        }
        UserAppReadinessObservation::NoCompute { detail } => {
            // 无运行中的计算资源：以控制意图与细节区分 stopping/stopped/not_deployed/starting。
            let status = match &intent {
                ControlIntent::StopAccepted => UserAppReadinessStatus::Stopping,
                ControlIntent::Starting => UserAppReadinessStatus::Starting,
                ControlIntent::None => match detail.as_deref() {
                    Some("scaled-to-zero") | Some("container-stopped") | Some("stopped") => {
                        UserAppReadinessStatus::Stopped
                    }
                    Some("no-pod-ip") | Some("scheduling") => UserAppReadinessStatus::Starting,
                    // 部署缺失且无启停意图：目标环境未部署。
                    _ => UserAppReadinessStatus::NotDeployed,
                },
            };
            let reason = match status {
                UserAppReadinessStatus::Stopping => Some(UserAppReadinessReason::StopAccepted),
                UserAppReadinessStatus::Starting => Some(UserAppReadinessReason::ServiceStarting),
                _ => None,
            };
            base_unknown(status, reason)
        }
        UserAppReadinessObservation::Snapshot { snapshot, .. } => {
            // 业务快照为准；Stop 已受理时旧 ready 不可沿（即使 Pod UID 未变）。
            let (status, reason) =
                if intent == ControlIntent::StopAccepted && snapshot.status.is_ready() {
                    (
                        UserAppReadinessStatus::Stopping,
                        Some(UserAppReadinessReason::StopAccepted),
                    )
                } else {
                    (snapshot.status, snapshot.reason_code)
                };
            UserAppReadinessResponse {
                app_id: app_id.to_string(),
                app_stage: app_stage.as_str().to_string(),
                ready: status.is_ready(),
                status,
                reason_code: reason,
                checked_at: snapshot.checked_at,
                runtime_instance_id: snapshot.runtime_instance_id,
                serving_release_id: snapshot.serving_release_id,
                target_release_id: snapshot.target_release_id,
                operation_id,
                runtime_operation_id: snapshot.operation_id,
                observation_revision: snapshot.observation_revision,
                proxy: snapshot.proxy,
                services: snapshot.services,
            }
        }
    }
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
            Some(&stop),
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
            None,
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
            Some(&stop),
            UserAppReadinessObservation::NoCompute { detail: None },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopping);

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            None,
            UserAppReadinessObservation::NoCompute {
                detail: Some("scaled-to-zero".into()),
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::Stopped);

        let response = merge_observation(
            "194",
            UserappStage::Dev,
            None,
            UserAppReadinessObservation::NoCompute {
                detail: Some("builder-missing".into()),
            },
        );
        assert_eq!(response.status, UserAppReadinessStatus::NotDeployed);

        let response = merge_observation(
            "194",
            UserappStage::Prod,
            Some(&start),
            UserAppReadinessObservation::NoCompute {
                detail: Some("deployment-missing".into()),
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
            None,
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
            None,
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
            Some(&stop),
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
}

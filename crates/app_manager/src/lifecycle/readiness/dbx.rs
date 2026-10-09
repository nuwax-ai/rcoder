//! `/dbx/readiness` 查询路径：dbx 探测 + 存储控制读取 → 响应组装。
//! 只读不变量与总预算钳制在此层收口。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use shared_types::{UserAppContainerReadiness, UserappStage};
use tokio::time::Instant;

use crate::error::AppOperationError;
use crate::models::AppResult;

use super::merge::{ReadinessControl, container_readiness, dbx_container_observation};

/// Poll the authoritative read inside the same deadline as the runtime query.
/// Keeping the read as a future also permits a stalled-store regression without
/// replacing the production lifecycle store or changing its mutation contract.
pub(super) async fn query_dbx_with_budget(
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::UserAppLifecycleStore;
    use shared_types::{UserAppContainerStatus, UserAppNoComputeState, UserAppOperationScope};
    use std::sync::atomic::{AtomicU8, Ordering};

    /// dbx readiness：prober 三态映射 + 探测失败降级 unknown（带原因码）+
    /// 权威记录缺失 404 + 全程只读（runtime 零写调用）。
    #[tokio::test]
    async fn dbx_readiness_maps_probe_outcomes_and_missing_app() {
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
        // 预算在存储读取阶段耗尽：无控制事实，容器不得伪造计算状态。
        assert_eq!(response.container.status, UserAppContainerStatus::Unknown);
        assert!(response.container.operation.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

// ============================================================================
// 陈旧 Running EnsureBuilder 证据化收束测试（生产 app 50/128 形态）
// ============================================================================

use super::fence_settler_tests::{
    FenceRuntime, fence_state, runtime_state_with_config, settled, stale_running_state,
};
use crate::app_state::AppState;
use container_runtime_api::UnboundBuilderCreationAuthority;
use shared_types::{UserAppAdmission, UserAppAdmissionOutcome, UserAppOperationKind};
use std::sync::Arc;

use super::stale_running::reconcile_stale_running_creation;

fn authority(runtime: &FenceRuntime, value: UnboundBuilderCreationAuthority) {
    *runtime.unbound_creation_authority.lock().expect("lock") = Some(value);
}

/// 正向（生产事故形态）：权威过期 + 同生命周期存活 builder → Failed 收束、
/// slot 释放、原入口可再受理。
#[tokio::test]
async fn stale_running_settles_with_expired_authority_and_live_builder() {
    let operation_id = "op-stale-live";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
    authority(
        &runtime,
        UnboundBuilderCreationAuthority::ExpiredOriginal {
            holder: "executor-fenced:op-stale-live".into(),
        },
    );
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, shared_types::UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"],
        "stale_running_authority_lost"
    );
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["authority"]["state"],
        "expired_original"
    );
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["builder"]["state"],
        "live"
    );
    assert!(
        record
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("stale running released")),
        "{:?}",
        record.error_message
    );
    // dev slot 已释放：同 scope 新受理 Accepted。
    let admitted = state
        .userapp_store
        .admit_with_input(
            &UserAppAdmission {
                app_id: "fenced".into(),
                lifecycle_id: Some(record.lifecycle_id.clone()),
                operation_id: format!("{operation_id}-retry"),
                request_id: Some(format!("req-{operation_id}-retry")),
                request_fingerprint: "cd".repeat(32),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            None,
        )
        .await
        .expect("admit after settle");
    assert!(matches!(admitted, UserAppAdmissionOutcome::Accepted(_)));
}

/// 权威缺席 + builder 后端正向证实缺席 → 同样收束（后续走正常 ensure 或
/// retained-storage 恢复，不在观察阶段重放创建）。
#[tokio::test]
async fn stale_running_settles_when_authority_absent_and_builder_proven_absent() {
    let operation_id = "op-stale-absent";
    let runtime = FenceRuntime::scenario(None, None);
    runtime
        .builder_absent_proven
        .store(true, std::sync::atomic::Ordering::SeqCst);
    authority(&runtime, UnboundBuilderCreationAuthority::Absent);
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, shared_types::UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["authority"]["state"],
        "absent"
    );
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["builder"]["state"],
        "absent_proven"
    );
}

/// 活权威（原生互斥仍被本执行身份续约）：恢复线程不得抢占，保持 Running。
#[tokio::test]
async fn stale_running_kept_while_original_authority_active() {
    let operation_id = "op-stale-active";
    let runtime = FenceRuntime::scenario(None, None);
    authority(&runtime, UnboundBuilderCreationAuthority::ActiveOriginal);
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, shared_types::UserAppOperationState::Running);
    assert!(record.checkpoint.get("fence_released_evidence").is_none());
}

/// 外来权威（互斥对象属其他执行）：不可判断、不可释放，保持。
#[tokio::test]
async fn stale_running_kept_for_foreign_authority() {
    let operation_id = "op-stale-foreign";
    let runtime = FenceRuntime::scenario(None, None);
    authority(
        &runtime,
        UnboundBuilderCreationAuthority::Foreign {
            holder: Some("other-executor:other-op".into()),
        },
    );
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::Running
    );
}

/// 后端不支持免回执观测（默认）：保守保持，不空转放行。
#[tokio::test]
async fn stale_running_kept_when_authority_observation_unsupported() {
    let operation_id = "op-stale-unsupported";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::Running
    );
}

/// 权威观测传输失败：未知 ≠ 死亡，保持。
#[tokio::test]
async fn stale_running_kept_when_authority_observation_fails() {
    let operation_id = "op-stale-transport";
    let runtime = FenceRuntime::scenario(None, None);
    runtime
        .unbound_authority_transport_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::Running
    );
}

/// 副作用不确定（builder 非活体且缺席未被证实）：保持。
#[tokio::test]
async fn stale_running_kept_when_builder_state_indefinite() {
    let operation_id = "op-stale-indefinite";
    let runtime = FenceRuntime::scenario(None, None);
    authority(&runtime, UnboundBuilderCreationAuthority::Absent);
    // builder_absent_proven=false 且无 workload → 非活体、未证实缺席。
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::Running
    );
}

/// 合法确认窗口内（claimed revision 年龄 < ensure_timeout）：不进入取证，
/// 保持 Running——活跃执行者不被恢复线程抢占。
#[tokio::test]
async fn stale_running_kept_within_confirmation_window() {
    let operation_id = "op-stale-fresh";
    let runtime = FenceRuntime::scenario(None, None);
    authority(
        &runtime,
        UnboundBuilderCreationAuthority::ExpiredOriginal {
            holder: "executor-fenced:op-stale-fresh".into(),
        },
    );
    // ensure_timeout=90s，记录刚 claim（年龄 ~0）→ 窗口内。
    let (state, _dir) = stale_running_state(runtime, operation_id, 90).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("scan");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::Running
    );
}

/// 窄门锁：RecoveryRequired 记录不经陈旧 Running 路径（围栏 settler 保有
/// 原语义）。调用后状态不变。
#[tokio::test]
async fn stale_running_path_ignores_recovery_required_records() {
    let operation_id = "op-stale-fenced";
    let runtime = FenceRuntime::scenario(None, None);
    authority(
        &runtime,
        UnboundBuilderCreationAuthority::ExpiredOriginal {
            holder: "executor-fenced:op-stale-fenced".into(),
        },
    );
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    assert_eq!(
        snapshot.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("no-op");
    assert_eq!(
        settled(&state, operation_id).await.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
}

/// 快照过期（并发推进）：锁下重读不等即放弃，不触碰新状态。
#[tokio::test]
async fn stale_running_settle_skipped_when_record_advanced_concurrently() {
    let operation_id = "op-stale-advanced";
    let runtime = FenceRuntime::scenario(None, None);
    authority(
        &runtime,
        UnboundBuilderCreationAuthority::ExpiredOriginal {
            holder: "executor-fenced:op-stale-advanced".into(),
        },
    );
    let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let mut stale_snapshot = settled(&state, operation_id).await;
    // 并发推进：另一个进度把 step 推到 creation_source_captured。
    let advanced = state
        .userapp_store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: stale_snapshot.app_id.clone(),
            lifecycle_id: stale_snapshot.lifecycle_id.clone(),
            operation_id: stale_snapshot.operation_id.clone(),
            expected_revision: stale_snapshot.revision,
            executor_id: "executor-fenced".into(),
            state: shared_types::UserAppOperationState::Running,
            step: "creation_source_captured".into(),
            checkpoint: stale_snapshot.checkpoint.clone(),
            error_code: None,
            error_message: None,
        })
        .await
        .expect("advance");
    stale_snapshot.revision = advanced.revision; // 过期快照模拟扫描竞态
    let snapshot = shared_types::UserAppOperationRecord {
        step: "claimed".into(), // 仍带旧 step，与库内不等
        ..stale_snapshot.clone()
    };
    reconcile_stale_running_creation(&state, &snapshot)
        .await
        .expect("skip");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, shared_types::UserAppOperationState::Running);
    assert_eq!(record.step, "creation_source_captured");
}

/// AppState 构建必须仍通过带配置入口（覆盖 ensure_timeout）——锁：默认
/// 配置路径不可回归为私有构造。
#[tokio::test]
async fn runtime_state_with_config_allows_ensure_timeout_override() {
    let runtime = FenceRuntime::scenario(None, None);
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("cfg.db"),
        )
        .await
        .expect("store"),
    );
    let mut config = crate::config::AppConfig::default();
    config.userapp_storage.ensure_timeout_seconds = 7;
    let state: Arc<AppState> = runtime_state_with_config(
        runtime,
        store,
        config,
        app_manager::config::AppAccessMode::Docker,
    )
    .await;
    assert_eq!(state.config.userapp_storage.ensure_timeout_seconds, 7);
}

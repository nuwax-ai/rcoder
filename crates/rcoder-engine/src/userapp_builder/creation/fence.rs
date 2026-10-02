use super::*;

/// 围栏证据：物理状态只读观察结论。
pub(crate) enum FenceEvidence {
    /// 状态确定且可观察（附留痕）——该生命周期已有一次完成的变更落地，
    /// 后继操作可安全重走完整 admission + capture + precondition 流程。
    Observed(serde_json::Value),
    /// 观察失败/身份不符/kind 无谓词：保持围栏（保守面不变）。
    Insufficient,
}

/// 发布族共用证据：活代 generation + 运行相位。generation == 本操作 =
/// 本次 rollout 已落地（app 154 事故形态：patch 后零 pod 窗口捕获失败，
/// rollout 事后照常完成）；generation 属后来操作 = 被取代（与 builder 族
/// "被另一次操作 ensure"同义）。维持 env 代次 token 口径，勿改
/// `.metadata.generation`（restart 单写会 bump generation 造成误判）。
pub(crate) async fn observe_generation_fence(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> FenceEvidence {
    let Ok(spec) = state.runtime().get_app_container_spec(&record.app_id).await else {
        return FenceEvidence::Insufficient;
    };
    let generation = spec
        .env
        .as_ref()
        .and_then(|env| env.get(shared_types::APP_DEPLOY_GENERATION_ID).cloned());
    let Some(generation) = generation else {
        return FenceEvidence::Insufficient;
    };
    let superseded = generation != record.operation_id;
    match state.app_service.get_app(&record.app_id).await {
        Ok(info) if info.phase == "Running" => FenceEvidence::Observed(serde_json::json!({
            "kind": if superseded { "superseded_by_newer_generation" }
                      else { "rollout_completed" },
            "live_generation": generation,
            "phase": info.phase,
            "ready_replicas": info.ready_replicas,
            "observed_at_us": chrono::Utc::now().timestamp_micros(),
        })),
        // 查询失败或相位未定：保持围栏。
        _ => FenceEvidence::Insufficient,
    }
}

/// per-kind 证据谓词。判定规则统一为：**物理状态确定（无死执行者在途写）**，
/// 而非"操作目标已达成"——围栏的职责是防半应用状态上的并发变更，而围栏
/// 落盘时执行者已携带已知错误返回（`OwnedOperation::fail`），不存在在途
/// 写；后继操作自带 admission/capture/precondition 防线。
///
/// **穷尽 match（禁通配臂）**：24 个 kind 全部显式列出——新增 kind 时编译
/// 器强制在此决定证据语义，避免静默落入"无谓词永久围栏"（09-22 用户纪律）。
pub(crate) async fn observe_fence_evidence(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> FenceEvidence {
    match record.kind {
        // dev 族：同 app+lifecycle 的存活 builder（指纹无关观察路径——
        // 被本生命周期另一次操作 ensure 出来的 builder 同样算数，正是
        // "被取代"情形）。
        UserAppOperationKind::EnsureBuilder | UserAppOperationKind::AdoptBuilder => {
            let context = shared_types::UserAppExecutionContext {
                app_id: record.app_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                executor_id: record
                    .executor_id
                    .clone()
                    .unwrap_or_else(|| "fenced-settler".into()),
                request_fingerprint: record.request_fingerprint.clone(),
            };
            match crate::userapp_builder::adoption::capture_bound_target(state, &context).await {
                // 活体工作负载是唯一可接受证据：identity 不符（Err）或
                // workload 缺失（Ok(None)）都保持围栏。
                Ok(target) if target.workload.is_some() => {
                    FenceEvidence::Observed(serde_json::json!({
                        "kind": "live_builder",
                        "workload": target.workload,
                        "observed_at_us": chrono::Utc::now().timestamp_micros(),
                    }))
                }
                _ => FenceEvidence::Insufficient,
            }
        }
        UserAppOperationKind::StartDeployment | UserAppOperationKind::RestartDeployment => {
            observe_generation_fence(state, record).await
        }
        // Create/Update：什么都没建出来（无部署痕迹）= no_trace（操作留下的
        // 痕迹全无，可安全关闭，app-166 的 prod 面同族）；有部署痕迹 →
        // 与发布族同一 env 代次口径判定。
        UserAppOperationKind::Create | UserAppOperationKind::Update => {
            match state.runtime().get_deployment_status(&record.app_id).await {
                Ok(None) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "no_trace",
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Ok(Some(_)) => observe_generation_fence(state, record).await,
                Err(_) => FenceEvidence::Insufficient,
            }
        }
        // prod 启停族：相位确定即收束（记录实际相位留痕——目标未达成时
        // 后继显式操作会重新观测并执行，好于永久挡死）。
        UserAppOperationKind::Start | UserAppOperationKind::Restart => {
            match state.app_service.get_app(&record.app_id).await {
                Ok(info) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "prod_phase_definite",
                    "phase": info.phase,
                    "replicas": info.replicas,
                    "ready_replicas": info.ready_replicas,
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Err(_) => FenceEvidence::Insufficient,
            }
        }
        UserAppOperationKind::Stop => match state.app_service.get_app(&record.app_id).await {
            Ok(info) if info.replicas == 0 => FenceEvidence::Observed(serde_json::json!({
                "kind": "prod_stopped",
                "phase": info.phase,
                "observed_at_us": chrono::Utc::now().timestamp_micros(),
            })),
            Ok(info) => FenceEvidence::Observed(serde_json::json!({
                "kind": "prod_still_running",
                "phase": info.phase,
                "replicas": info.replicas,
                "observed_at_us": chrono::Utc::now().timestamp_micros(),
            })),
            Err(_) => FenceEvidence::Insufficient,
        },
        // SetRecyclePolicy：回收注解可读即确定状态（同 Stop 哲学）——
        // desired/observed 双留痕，目标未达成时后继显式操作重放。
        UserAppOperationKind::SetRecyclePolicy => {
            let Some(shared_types::UserAppControlCommand::SetRecyclePolicy { policy }) =
                &record.command
            else {
                return FenceEvidence::Insufficient;
            };
            match state.runtime().get_deployment_status(&record.app_id).await {
                Ok(Some(info)) => {
                    let desired_hit = policy
                        .recycle_enabled
                        .is_none_or(|v| Some(v) == info.recycle_enabled)
                        && policy
                            .idle_timeout_seconds
                            .is_none_or(|v| Some(v) == info.idle_timeout_seconds)
                        && policy
                            .wake_on_traffic
                            .is_none_or(|v| Some(v) == info.wake_on_traffic);
                    FenceEvidence::Observed(serde_json::json!({
                        "kind": if desired_hit { "already_at_desired" } else { "policy_differs" },
                        "desired": policy,
                        "observed": {
                            "recycle_enabled": info.recycle_enabled,
                            "idle_timeout_seconds": info.idle_timeout_seconds,
                            "wake_on_traffic": info.wake_on_traffic,
                        },
                        "observed_at_us": chrono::Utc::now().timestamp_micros(),
                    }))
                }
                Ok(None) | Err(_) => FenceEvidence::Insufficient,
            }
        }
        // Deletion can include detached dev/storage writes. A deployment's
        // presence or absence does not retire those writes; its dedicated
        // recovery path verifies every original captured target instead.
        UserAppOperationKind::DeleteCompute
        | UserAppOperationKind::PurgeResources
        | UserAppOperationKind::DeleteApplication => FenceEvidence::Insufficient,
        // 以下 kind 无只读可判定证据（存储深度/密码远端回执/热部署收敛/
        // 杂项控制）——保守保持围栏，超龄 + holder 死亡由 holder_expired
        // 兜底有界收束。**显式列出（禁通配臂）**：新增 kind 必须在此决定
        // 证据语义。
        UserAppOperationKind::AdoptApplication
        | UserAppOperationKind::HotDeploy
        | UserAppOperationKind::StopBuilder
        | UserAppOperationKind::RestartBuilder
        | UserAppOperationKind::DestroyDevStorage
        | UserAppOperationKind::DestroyProdStorage
        | UserAppOperationKind::ClearDevStorage
        | UserAppOperationKind::ClearProdStorage
        | UserAppOperationKind::ResetDevDatabasePassword
        | UserAppOperationKind::ResetProdDatabasePassword
        | UserAppOperationKind::PrepareProdDatabase => FenceEvidence::Insufficient,
    }
}

/// holder 死亡兜底的收束留痕说明。
const HOLDER_EXPIRED_RELEASE_NOTE: &str =
    "holder expired, outcome unverifiable; retry the operation";
/// 物理状态确定性观察收束的留痕说明。
const DEFINITE_RELEASE_NOTE: &str = "physical state verified definite by recovery scanner";
/// 围栏 holder 死亡兜底的默认超龄门槛（秒）：≫ 操作 deadline + 租约 TTL 60s。
const DEFAULT_FENCE_SETTLE_GRACE_SECS: u64 = 900;

/// holder 死亡兜底判定（围栏有界保证）：**围栏超龄 ∧ 持有者已死**。
///
/// 持有者死亡证明：无租约绑定行（执行者从未取得锁/绑定已被终态清扫），
/// 或运行时死亡探针判定已死（[`container_runtime_api::UserAppDeploymentRuntime::app_operation_receipt_holder_dead`]：
/// K8s=TTL 过期/对象缺失/被接管；Docker=flock 已被内核释放/身份被替换）。
/// 探针极性各后端不同，不得由 validate 推导（Docker flock 的 Ok(true) 是
/// 孤儿 marker 的 authority 残留，推导会把活锁判死、把孤儿判活）。
/// 围栏落盘时执行者已携带已知错误返回（`OwnedOperation::fail`），不存在
/// 正在途写的持有者；探针失败/租约仍持有/未超龄 → 保守保持围栏。
/// 收束一律 Failed（`settle_fenced_operation`），绝不伪造 Succeeded。
pub(crate) async fn holder_expired(state: &AppState, record: &UserAppOperationRecord) -> bool {
    if matches!(
        record.kind,
        UserAppOperationKind::DeleteCompute
            | UserAppOperationKind::PurgeResources
            | UserAppOperationKind::DeleteApplication
    ) {
        // Losing lease authority does not retract an already-issued DELETE.
        return false;
    }
    let grace_secs = state
        .config
        .fence_settle_grace_secs
        .unwrap_or(DEFAULT_FENCE_SETTLE_GRACE_SECS);
    let grace_us = i64::try_from(grace_secs.saturating_mul(1_000_000)).unwrap_or(i64::MAX);
    let now_us = chrono::Utc::now().timestamp_micros();
    // 计龄从 created_at 起算（记录仅暴露 created_at；created ≤ updated，
    // 保守取更长的等待）。
    if now_us.saturating_sub(record.created_at.timestamp_micros()) < grace_us {
        return false;
    }
    match state
        .userapp_store
        .get_operation_lease(&record.app_id, &record.operation_id)
        .await
    {
        Ok(None) => true,
        Ok(Some(binding)) => {
            match state
                .runtime()
                .app_operation_receipt_holder_dead(&binding.context, &binding.receipt)
                .await
            {
                Ok(dead) => dead,
                Err(error) => {
                    tracing::warn!(
                        operation_id = %record.operation_id,
                        app_id = %record.app_id,
                        %error,
                        "Fence kept: holder death probe failed"
                    );
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                %error,
                "Fence kept: holder death check failed to read lease binding"
            );
            false
        }
    }
}

/// Auto-settle a fenced operation whose physical state is verifiably definite:
/// the interrupted operation is settled as Failed with observation evidence —
/// never Succeeded, because this operation itself did not complete — which
/// frees the admission slot so the next explicit operation proceeds normally.
/// Covers every kind with an evidence predicate (test-env app 151/155: fenced
/// EnsureBuilder blocked every chat with "Container operation failed"; app
/// 154: fenced start_deployment blocked redeploy with ERR_CONFLICT until an
/// operator ran manual SQL). Identity mismatch or absent resources keep the
/// fence protected.
pub(crate) async fn reconcile_fenced_ensure(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<()> {
    if snapshot.state != UserAppOperationState::RecoveryRequired {
        return Ok(());
    }
    if matches!(
        snapshot.kind,
        UserAppOperationKind::DeleteCompute
            | UserAppOperationKind::PurgeResources
            | UserAppOperationKind::DeleteApplication
    ) {
        state
            .app_service
            .reconcile_interrupted_deletion(snapshot)
            .await?;
        return Ok(());
    }
    let Some(_local) = crate::userapp_builder::lifecycle::try_acquire(&snapshot.app_id).await
    else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Fence settle skipped: app lifecycle lock is contended this tick"
        );
        return Ok(());
    };
    // Re-read under the lock: another executor may have settled or advanced
    // the operation since the scan snapshot was taken.
    let current = state
        .userapp_store
        .get_operation(&snapshot.app_id, &snapshot.operation_id)
        .await?;
    let Some(record) = current.filter(|record| {
        record.state == UserAppOperationState::RecoveryRequired && record.kind == snapshot.kind
    }) else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Fence settle skipped: record advanced or settled concurrently"
        );
        return Ok(());
    };
    // 无执行者声明的记录不可被扫描器收束（store 侧同样拒绝）。
    if record.executor_id.is_none() {
        tracing::warn!(
            operation_id = %record.operation_id,
            app_id = %record.app_id,
            "Fence kept: record has no executor claim"
        );
        return Ok(());
    }
    // 只读观察整体限时：kube 请求无总超时（kube-rs 默认 read_timeout=None），
    // 一个 stall 连接会把恢复任务永久挂死，占满扫描器 8 槽后瘫痪整个
    // 恢复管线（0.1.288 线上 20 围栏零收束的根因形态）。超时=证据不足，
    // 保守保持围栏，下一扫描周期重试。
    let observation = tokio::time::timeout(
        Duration::from_secs(10),
        observe_fence_evidence(state, &record),
    )
    .await;
    let evidence = match observation {
        Ok(FenceEvidence::Observed(evidence)) => evidence,
        Err(_) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                "Fence kept: evidence observation timed out (stalled runtime call)"
            );
            return Ok(());
        }
        Ok(FenceEvidence::Insufficient) => {
            // 线上定位锚点：此日志出现说明分发与执行都正常、卡在证据谓词
            // （观察失败或身份不符）。每 app 每扫描周期一条，节流由扫描
            // 器节奏（5s）天然限定。
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                kind = ?record.kind,
                step = %record.step,
                "Fence kept: evidence predicate returned insufficient"
            );
            // holder 死亡兜底：无据可查但持有者已死（无绑定/租约过期）且
            // 围栏超龄 → 有界收束，不再永久占受理 slot（app-166 事故类：
            // 创建报错落围栏后什么都没建出来，live 证据谓词永不满足）。
            if holder_expired(state, &record).await {
                let evidence = serde_json::json!({
                    "kind": "holder_expired",
                    "checked_at_us": chrono::Utc::now().timestamp_micros(),
                });
                state
                    .userapp_store
                    .settle_fenced_operation(&record, &evidence, HOLDER_EXPIRED_RELEASE_NOTE)
                    .await?;
                tracing::warn!(
                    operation_id = %record.operation_id,
                    app_id = %record.app_id,
                    kind = ?record.kind,
                    "Fenced operation settled as Failed: holder expired, outcome unverifiable"
                );
            }
            return Ok(());
        }
    };
    // 经 store 的 sanctioned 终态化路径（内部 Running 跳转满足状态机独占
    // 门，纯记账不授权运行时工作）——直接 advance(Failed) 会被
    // domain::advance 的 RecoveryRequired 转移门拒绝（f49b594d 潜伏 bug：
    // settler 从未真正收束过任何围栏）。
    state
        .userapp_store
        .settle_fenced_operation(&record, &evidence, DEFINITE_RELEASE_NOTE)
        .await?;
    tracing::warn!(
        operation_id = %record.operation_id,
        app_id = %record.app_id,
        kind = ?record.kind,
        "Fenced operation settled as Failed: physical state verified definite"
    );
    Ok(())
}

//! 陈旧 Running EnsureBuilder 的证据化对账（生产 app 50/128 形态）。
//!
//! 历史记录在执行者死亡时可能既无完成回执、也无持久化租约绑定与
//! deadline（claim 后、任何 checkpoint/中断记录前死亡）。回执恢复对这类
//! 记录每轮返回 false，而围栏 settler 的状态门只接受 RecoveryRequired，
//! Running 无人转换 → dev slot 永久占用，构建/Git/文件列表全部被拒。
//!
//! 本路径在回执恢复失败后补齐收束证据：确认窗口已过 + 原执行权威确认
//! 失效（原生互斥对象无活性持有）+ 副作用状态确定（同生命周期存活
//! builder 或后端正向证实缺席），然后经专用存储 CAS 以 Failed 收束并
//! 释放 slot。绝不伪造 Succeeded；未知一律保持并留结构化原因。

use super::*;

/// 收束留痕说明：原执行权威已失效且无回执，重试原入口即可。
const STALE_AUTHORITY_RELEASE_NOTE: &str =
    "original creation authority expired without a receipt; retry the operation";
/// 权威与副作用观察的整体限时（与围栏 settler 同预算）：kube 请求无总
/// 超时，stall 连接不得挂死恢复任务；超时 = 证据不足，保持。
const STALE_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10);

/// 回执恢复失败后的陈旧 Running 对账入口。由恢复扫描器在
/// [`super::reconcile_runtime_receipt`] 返回 false 后调用；RecoveryRequired
/// 记录不经此路径（围栏 settler 保留其原语义与状态门）。
pub(crate) async fn reconcile_stale_running_creation(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<()> {
    // 窄门：仅 EnsureBuilder + Running。其余形态原样返回，不扩大到任意
    // Running 操作的通用收束。
    if snapshot.kind != UserAppOperationKind::EnsureBuilder
        || snapshot.state != UserAppOperationState::Running
        || !shared_types::userapp_builder_creation_needs_runtime_receipt(snapshot)
    {
        return Ok(());
    }
    // 确认窗口保护：复用回执恢复的同一判定（deadline 已过，或旧记录按
    // claimed revision 的更新时间超过 ensure 预算）。活跃/预算内的原执行
    // 不被恢复线程抢占。
    if !running_confirmation_may_be_recovered(state, snapshot).await? {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Stale Running kept: original confirmation window has not elapsed"
        );
        return Ok(());
    }
    let Some(_local) = crate::userapp_builder::lifecycle::try_acquire(&snapshot.app_id).await
    else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Stale Running settle skipped: app lifecycle lock is contended this tick"
        );
        return Ok(());
    };
    // 锁下重读：快照必须仍精确成立（并发收束/推进由存储 CAS 最终兜底）。
    let current = state
        .userapp_store
        .get_operation(&snapshot.app_id, &snapshot.operation_id)
        .await?;
    let Some(record) = current.filter(|record| record == snapshot) else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Stale Running settle skipped: record advanced or settled concurrently"
        );
        return Ok(());
    };
    // 无执行者声明的记录不可被扫描器收束（存储侧同样拒绝）。
    let Some(executor_id) = record.executor_id.clone() else {
        tracing::warn!(
            operation_id = %record.operation_id,
            app_id = %record.app_id,
            "Stale Running kept: record has no executor claim"
        );
        return Ok(());
    };
    let context = shared_types::UserAppExecutionContext {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        executor_id,
        request_fingerprint: record.request_fingerprint.clone(),
    };
    let observation = match tokio::time::timeout(
        STALE_OBSERVATION_TIMEOUT,
        observe_stale_creation_evidence(state, &context),
    )
    .await
    {
        Ok(Ok(observation)) => observation,
        Ok(Err(reason)) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                kind = ?record.kind,
                step = %record.step,
                reason = %reason,
                "Stale Running kept: settlement evidence is not conclusive"
            );
            return Ok(());
        }
        Err(_) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                "Stale Running kept: evidence observation timed out (stalled runtime call)"
            );
            return Ok(());
        }
    };
    state
        .userapp_store
        .settle_stale_running_builder(&record, &observation.evidence, STALE_AUTHORITY_RELEASE_NOTE)
        .await?;
    // Drop the retained physical exclusion before retryable terminal cleanup.
    // The binding was saved while Running, so a crash here remains discoverable.
    drop(observation.guard);
    if let Some(binding) = state
        .userapp_store
        .get_operation_lease(&record.app_id, &record.operation_id)
        .await?
    {
        state
            .runtime()
            .release_app_operation_receipt(&binding.context, &binding.receipt)
            .await?;
        state.userapp_store.forget_operation_lease(&binding).await?;
    }
    tracing::warn!(
        operation_id = %record.operation_id,
        app_id = %record.app_id,
        lifecycle_id = %record.lifecycle_id,
        kind = ?record.kind,
        step = %record.step,
        revision = record.revision,
        "Stale Running EnsureBuilder settled as Failed: original creation authority lost"
    );
    Ok(())
}

struct SettlementObservation {
    evidence: serde_json::Value,
    // Held across the storage CAS, not just the runtime observation.
    guard: Option<Box<dyn container_runtime_api::ComputeHandoffGuard>>,
}

/// 权威 + 副作用取证。`Err(reason)` 携带明确的保持原因（未知不当缺席）。
async fn observe_stale_creation_evidence(
    state: &AppState,
    context: &shared_types::UserAppExecutionContext,
) -> Result<SettlementObservation> {
    // 原执行权威：免回执地按持久身份发现原生互斥对象（K8s Lease /
    // Docker flock marker）。仅 ExpiredOriginal/Absent 证明该执行不再有
    // 活性持有；传输失败按未知处理。
    let (authority, holder, receipt) = match state
        .runtime()
        .observe_unbound_builder_creation_authority(context)
        .await
    {
        Ok(container_runtime_api::UnboundBuilderCreationAuthority::ExpiredOriginal {
            holder,
            receipt,
        }) => ("expired_original", holder, Some(receipt)),
        Ok(container_runtime_api::UnboundBuilderCreationAuthority::Absent) => {
            ("absent", String::new(), None)
        }
        Ok(container_runtime_api::UnboundBuilderCreationAuthority::ActiveOriginal) => {
            return Err(anyhow!("original creation authority is still active"));
        }
        Ok(container_runtime_api::UnboundBuilderCreationAuthority::Foreign { holder }) => {
            return Err(anyhow!(
                "native operation mutex belongs to another execution: {}",
                holder.unwrap_or_else(|| "<unidentified>".into())
            ));
        }
        Ok(container_runtime_api::UnboundBuilderCreationAuthority::Unsupported) => {
            return Err(anyhow!(
                "backend cannot observe authority without a persisted receipt"
            ));
        }
        Err(error) => {
            return Err(anyhow!("authority observation failed: {error:#}"));
        }
    };
    // Save the actual native identity BEFORE the terminal transaction. Docker
    // creation refuses orphan markers, so cleanup is required for reuse, and
    // it must remain discoverable after a crash between SQL and cleanup.
    let existing = state
        .userapp_store
        .get_operation_lease(&context.app_id, &context.operation_id)
        .await?;
    if let Some(existing) = &existing {
        if existing.context != *context
            || receipt.as_ref().is_some_and(|receipt| {
                !crate::userapp_builder::compute_drain_recovery::same_lease_identity(
                    &existing.receipt,
                    receipt,
                )
            })
        {
            return Err(anyhow!(
                "persisted native authority differs from the observed original"
            ));
        }
        // Keep the original cleanup receipt. K8s renewals change RV, not its
        // UID/token authority, and must not make the next scan fail rebinding.
    } else if let Some(receipt) = &receipt {
        state
            .userapp_store
            .bind_operation_lease(context, receipt)
            .await?;
    }
    let binding = state
        .userapp_store
        .get_operation_lease(&context.app_id, &context.operation_id)
        .await?;
    let retained = state
        .runtime()
        .prepare_compute_handoff(
            context,
            shared_types::UserAppOperationScope::Dev,
            binding.as_ref().map(|binding| &binding.receipt),
        )
        .await?;
    // Revalidate the native holder under physical exclusion. A renewal or a
    // replacement between the first GET and here must not release the slot.
    let guard = match retained {
        container_runtime_api::ComputeHandoff::Acquired { guard, .. } => Some(guard),
        container_runtime_api::ComputeHandoff::Vacant { .. } => None,
        container_runtime_api::ComputeHandoff::Held => {
            return Err(anyhow!("native holder became active before settlement"));
        }
        container_runtime_api::ComputeHandoff::Deferred(inspection) => {
            return Err(anyhow!(
                "native identity changed before settlement: {inspection:?}"
            ));
        }
    };
    // 副作用确定性：同生命周期存活 builder（与围栏谓词同一观察），或后端
    // 正向证实缺席。查询失败/身份不符/两者皆非 = 未知，保持。
    let builder = match crate::userapp_builder::adoption::capture_bound_target(state, context).await
    {
        Ok(target) if target.workload.is_some() => serde_json::json!({
            "state": "live",
            "workload": target.workload,
        }),
        Ok(_) => match state.runtime().builder_compute_absent(context).await {
            Ok(true) => serde_json::json!({"state": "absent_proven"}),
            Ok(false) => {
                return Err(anyhow!("builder is neither live nor proven absent"));
            }
            Err(error) => {
                return Err(anyhow!("builder absence probe failed: {error:#}"));
            }
        },
        Err(error) => {
            return Err(anyhow!("builder capture failed: {error:#}"));
        }
    };
    let evidence = serde_json::json!({
        "kind": "stale_running_authority_lost",
        "authority": {
            "state": authority,
            "holder": holder,
        },
        "builder": builder,
        "observed_at_us": chrono::Utc::now().timestamp_micros(),
    });
    Ok(SettlementObservation { evidence, guard })
}

//! A controller-created Pod successor is not a new workload adoption. Repair
//! only its registration, never stop it using the predecessor's idle evidence.
//! The same physical Pod drifting to a new address is repaired the same way,
//! except its activity clock is preserved: correcting a stale address is not
//! user activity and must never restart the idle observation window.
use super::{adoption, lifecycle, refreshed_registration, validate_builder_identity};
use crate::app_state::AppState;
use anyhow::{Context, Result, ensure};
use rcoder_storage::WorkloadAdmissionMode;
use shared_types::{
    ContainerBasicInfo, ProjectAndContainerInfo, ServiceType, UserAppOperationScope,
    persistence::PersistenceWriteOutcome,
};
use std::sync::Arc;

/// 登记快照与运行时真实身份的偏差分类（idle 扫描观察点）。
#[derive(Debug)]
enum BuilderRegistrationDrift {
    /// 同 UID 且地址一致：登记无需动作，扫描继续使用当前快照。
    Unchanged,
    /// 同 UID 但地址变化：只校正派生地址，保留原活动时钟与物理代次。
    AddressOnly,
    /// 同 controller 的新 Pod：按既有规则刷新登记并重开观察窗。
    Successor,
}

/// 分类观察点：运行时真实身份 vs 登记快照。
///
/// 不同物理 UID 只有同一 controller（同 workload_uid）才是受控后继；外实例
/// 或证据不足直接拒绝——缺证据既不是接管许可，也不是删除许可。
fn classify_registration_drift(
    previous: &ContainerBasicInfo,
    observed: &container_runtime_api::RuntimeContainerInfo,
) -> Result<BuilderRegistrationDrift> {
    if observed.container_id == previous.container_id {
        return Ok(if observed.container_ip == previous.container_ip {
            BuilderRegistrationDrift::Unchanged
        } else {
            BuilderRegistrationDrift::AddressOnly
        });
    }
    ensure!(
        previous
            .workload_uid
            .as_ref()
            .is_some_and(|uid| !uid.is_empty())
            && observed.workload_uid == previous.workload_uid,
        "Builder workload changed; idle observation cannot authorize adoption"
    );
    Ok(BuilderRegistrationDrift::Successor)
}

/// 失效旧地址派生的 gRPC 通道（仅地址修正路径调用：调用时已确认两地址不同）。
///
/// 扫描与状态查询按登记 IP 建连并缓存通道；地址收敛后旧通道指向已无 Pod 的
/// 地址，继续复用只会重复超时。按 addr 移除幂等，且不触碰 SSE 流——同代次
/// 地址修正没有退出任何物理执行。
async fn invalidate_stale_builder_address(state: &AppState, previous: &ContainerBasicInfo) {
    let stale = format!(
        "{}:{}",
        previous.container_ip,
        shared_types::GRPC_DEFAULT_PORT
    );
    state.grpc_pool.remove(&stale).await;
}

/// true means this scan must stop using its old project/physical snapshot.
/// No compute, lifecycle, binding, operation receipt or workspace is created.
pub(crate) async fn refresh_idle_builder_pod(
    state: &AppState,
    expected: &ProjectAndContainerInfo,
) -> Result<bool> {
    let budget = std::time::Duration::from_secs(
        state
            .config
            .cleanup_config
            .orphan_scan_budget_seconds
            .max(1),
    );
    tokio::time::timeout(budget, refresh_idle_builder_pod_inner(state, expected))
        .await
        .context("Builder idle registration observation remains unconfirmed")?
}

async fn refresh_idle_builder_pod_inner(
    state: &AppState,
    expected: &ProjectAndContainerInfo,
) -> Result<bool> {
    if expected.service_type() != Some(ServiceType::UserappBuilder) {
        return Ok(false);
    }
    let app_id = expected.project_id();
    let Some(previous) = expected.container_info() else {
        return Ok(false);
    };
    let expected_generation = expected.persistence_identity().generation.clone();
    let Some(observed) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(false);
    };
    let drift = classify_registration_drift(&previous, &observed)?;
    let address_only = matches!(drift, BuilderRegistrationDrift::AddressOnly);
    if matches!(drift, BuilderRegistrationDrift::Unchanged) {
        return Ok(false);
    }
    validate_builder_identity(app_id, &observed)?;
    let guard = state
        .acquire_agent_workload(
            app_id,
            &ServiceType::UserappBuilder,
            WorkloadAdmissionMode::Exclusive,
        )
        .await?;
    let Some(_local) = lifecycle::try_acquire(app_id).await else {
        return Ok(true);
    };
    state
        .userapp_store
        .check_compute_access(app_id, UserAppOperationScope::Dev, false)
        .await?;
    if crate::cleanup_task::cleaner::builder_fenced_by_active_operations(
        state.userapp_store.as_ref(),
        app_id,
    )
    .await
    {
        return Ok(true);
    }
    let Some(current) = state.projects.refresh_project_registration(app_id).await? else {
        return Ok(true);
    };
    let Some(canonical) = current.container_info() else {
        return Ok(true);
    };
    if current.persistence_identity().generation != expected_generation
        || canonical.container_id != previous.container_id
        || canonical.workload_uid != previous.workload_uid
    {
        return Ok(true);
    }
    let frame = state
        .projects
        .capture_workload_registration(&guard, guard.key().logical_name())
        .await?;
    ensure!(
        frame.project_ids() == vec![app_id]
            && frame.physical_uid() == Some(previous.container_id.as_str()),
        "Builder registration has different or shared canonical references"
    );
    let Some(actual) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(true);
    };
    validate_builder_identity(app_id, &actual)?;
    ensure!(
        actual.workload_uid == previous.workload_uid
            && actual.container_id == observed.container_id
            && actual.container_name == previous.container_name,
        "Builder changed while refreshing its idle registration"
    );
    adoption::verify_live_builder_observation(state, app_id, app_id, &actual).await?;
    let Some(updated) = refreshed_registration(&canonical, &actual) else {
        return Ok(true);
    };
    state
        .userapp_store
        .check_compute_access(app_id, UserAppOperationScope::Dev, false)
        .await?;
    guard.verify_current().await?;
    let mut project = (*current).clone();
    project.set_container(Some(updated));
    if address_only {
        // An address correction is not user activity: keep the original clock
        // so this Pod's idle observation continues instead of restarting, and
        // drop the channel cached for the address that no longer exists.
        invalidate_stale_builder_address(state, &previous).await;
    } else {
        // Activity/grace belongs to the new execution. Old idle evidence must never
        // cause its immediate retirement, even when the old project was very old.
        project.update_activity();
    }
    let outcome = state
        .projects
        .insert_project_registration_durable(app_id.into(), Arc::new(project))
        .await?;
    match outcome {
        PersistenceWriteOutcome::Committed => {
            if address_only {
                tracing::info!(app_id,
                    old_address = %previous.container_ip, current_address = %actual.container_ip,
                    pod = %actual.container_id,
                    "Builder Pod address registration converged; idle observation clock preserved");
            } else {
                tracing::info!(app_id,
                    old_pod = %previous.container_id, current_pod = %actual.container_id,
                    "Builder Pod registration refreshed; successor starts a new idle observation window");
            }
        }
        PersistenceWriteOutcome::Superseded => {}
        PersistenceWriteOutcome::Deferred { reason } => {
            anyhow::bail!("Builder Pod registration update remains unconfirmed: {reason}")
        }
    }
    Ok(true)
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    use container_runtime_api::{ContainerRuntimeStatus, RuntimeContainerInfo};
    use shared_types::ServiceType as Family;

    fn registered(ip: &str, uid: &str, workload: Option<&str>) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: uid.into(),
            container_name: "rcoder-app-builder-102".into(),
            container_ip: ip.into(),
            internal_port: 60000,
            external_port: 0,
            project_id: "102".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: format!("http://{ip}:60000"),
            workload_uid: workload.map(str::to_string),
        }
    }

    fn observed(ip: &str, uid: &str, workload: Option<&str>) -> RuntimeContainerInfo {
        RuntimeContainerInfo {
            container_id: uid.into(),
            container_name: "rcoder-app-builder-102".into(),
            container_ip: ip.into(),
            status: ContainerRuntimeStatus::Running,
            created_at: chrono::Utc::now(),
            env_vars: None,
            service_type: Some(Family::UserappBuilder),
            project_id: Some("102".into()),
            user_id: None,
            pod_id: None,
            app_id: Some("102".into()),
            workload_uid: workload.map(str::to_string),
        }
    }

    /// 同 UID 且地址一致：无需动作（扫描继续使用当前快照）。
    #[test]
    fn same_pod_same_address_needs_no_action() {
        assert!(matches!(
            classify_registration_drift(
                &registered("10.42.4.207", "pod-uid", Some("sts-uid")),
                &observed("10.42.4.207", "pod-uid", Some("sts-uid"))
            )
            .unwrap(),
            BuilderRegistrationDrift::Unchanged
        ));
    }

    /// 同 UID 但地址变化（现场 102）：只校正派生地址，不是新代次。
    #[test]
    fn same_pod_new_address_is_address_only() {
        assert!(matches!(
            classify_registration_drift(
                &registered("10.42.4.17", "pod-uid", Some("sts-uid")),
                &observed("10.42.4.207", "pod-uid", Some("sts-uid"))
            )
            .unwrap(),
            BuilderRegistrationDrift::AddressOnly
        ));
    }

    /// 同 controller 的新 Pod：按既有规则刷新登记并重开观察窗。
    #[test]
    fn same_controller_new_pod_is_successor() {
        assert!(matches!(
            classify_registration_drift(
                &registered("10.42.4.17", "old-pod", Some("sts-uid")),
                &observed("10.42.4.207", "new-pod", Some("sts-uid"))
            )
            .unwrap(),
            BuilderRegistrationDrift::Successor
        ));
    }

    /// 不同 controller：外实例，明确拒绝而非按后继处理。
    #[test]
    fn foreign_controller_is_refused() {
        let error = classify_registration_drift(
            &registered("10.42.4.17", "old-pod", Some("sts-uid")),
            &observed("10.42.4.207", "new-pod", Some("foreign-sts")),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("Builder workload changed"),
            "foreign controller must be refused with a specific diagnostic"
        );
    }

    /// 缺 workload_uid 证据（Docker 容器或标签缺失）：不按后继处理。
    #[test]
    fn missing_workload_evidence_is_refused() {
        assert!(
            classify_registration_drift(
                &registered("10.42.4.17", "old-pod", None),
                &observed("10.42.4.207", "new-pod", None)
            )
            .is_err()
        );
    }
}

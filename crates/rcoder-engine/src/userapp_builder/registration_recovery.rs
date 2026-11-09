//! Controlled recovery of a MISSING canonical builder registration.
//!
//! A live UserApp builder whose `projects` row is absent is invisible to the
//! ordinary idle scanner (it only walks `projects`), so the reverse inventory
//! currently protects it forever as `LifecycleReferenced`. This module closes
//! that gap without creating a second reaper and without weakening ownership:
//!
//! * authority is either a **successful, identity-bound creation/Ensure
//!   receipt** (the existing durable chain) or a **valid resource binding for
//!   the current lifecycle** — the same evidence that protects the workload
//!   today;
//! * nothing is created, started, restarted or migrated: the physical Pod,
//!   controller, PVC and data are only observed;
//! * insufficient evidence returns `Ok(None)`; the caller retains the
//!   resource and reports the specific gap. Missing evidence is neither
//!   permission to adopt nor permission to delete.
use super::{adoption, creation, validate_builder_identity};
use crate::app_state::AppState;
use anyhow::{Context, Result};
use shared_types::{
    AGENT_FILE_SERVER_PORT, ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore as _,
    ServiceType, UserAppLifecycleState, persistence::PersistenceWriteOutcome,
};
use std::sync::Arc;

/// 登记缺口为何本轮无法闭环。**受控枚举**：日志与指标只允许这些值，不按
/// 用户/项目/操作 ID 展开，避免高基数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistrationGap {
    /// 生命周期不在 Active、运行时未见、非 Running、管理 Service 缺席，或历史
    /// 标识/标签不一致（不猜 app id）。
    NotObserved,
    /// 在途 dev/application 操作围栏：登记归属在途操作。
    OperationInFlight,
    /// 受理的“仅登记确认”被拒绝（操作 Failed）。确定性 request id 绑定
    /// 物理证据，不自动重试：证据变化（换代）或显式人工确认再进。
    ConfirmationRejected,
    /// 无当前生命周期对观察到的 controller 的有效绑定。
    BindingMissing,
    /// 卷校验未通过或证据缺失；也可能是校验中的生命周期/资源视图变化，
    /// 不能仅凭此分类断言原卷已经损坏或被替换。
    VolumeUnverified,
    /// 登记写入未被确认（并发写入胜出，或 Project Stop 正在等待持久化确认）。
    /// 结果未知，既不算成功也不算失败。
    WriteNotConfirmed,
}

impl RegistrationGap {
    /// 有界原因集合（聚合计数的下标来源，新增必须同步）。
    pub(crate) const ALL: [Self; 6] = [
        Self::NotObserved,
        Self::OperationInFlight,
        Self::ConfirmationRejected,
        Self::BindingMissing,
        Self::VolumeUnverified,
        Self::WriteNotConfirmed,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotObserved => "not_observed",
            Self::OperationInFlight => "operation_in_flight",
            Self::ConfirmationRejected => "confirmation_rejected",
            Self::BindingMissing => "binding_missing",
            Self::VolumeUnverified => "volume_unverified",
            Self::WriteNotConfirmed => "write_not_confirmed",
        }
    }
}

/// 受控恢复结果：`Completed` = 登记已落库（可被普通 scanner 遍历）；
/// `Unconfirmed` = 本轮证据不足，资源保留并需显式受控恢复入口。
#[derive(Debug, PartialEq)]
pub(crate) enum BuilderRegistrationRecovery {
    Completed(ContainerBasicInfo),
    Unconfirmed(RegistrationGap),
}

/// 补齐缺失的 canonical builder 登记。
pub(crate) async fn recover_missing_builder_registration(
    state: &AppState,
    app_id: &str,
) -> Result<BuilderRegistrationRecovery> {
    // A missing lifecycle record is an expected reconciliation outcome (the
    // workload may be protected only by a binding or a compute control), not a
    // programming error: report it as a gap instead of failing the scan round.
    let Some(app) = state.userapp_store.get_application(app_id).await? else {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::NotObserved,
        ));
    };
    if app.state != UserAppLifecycleState::Active {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::NotObserved,
        ));
    }
    // Read-only runtime observation: the same source the ordinary path uses.
    let Some(observed) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await
        .context("inspect builder without canonical registration")?
    else {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::NotObserved,
        ));
    };
    validate_builder_identity(app_id, &observed)?;
    if observed.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::NotObserved,
        ));
    }
    // Real runtime identity, current application/lifecycle and workload UID. The
    // verified target is reused for the volume check below instead of capturing
    // the same controller/Pod a second time.
    let verified_target =
        adoption::verify_live_builder_observation(state, app_id, app_id, &observed).await?;
    // An in-flight dev/application operation owns this application's
    // registration decision (Stop/Restart/Ensure/Destroy/ClearDevStorage/Purge).
    // A background reconciliation must never race it into a row for a Pod that
    // the operation is replacing or retiring. An unreadable fence also defers.
    if crate::cleanup_task::cleaner::builder_fenced_by_active_operations(
        state.userapp_store.as_ref(),
        app_id,
    )
    .await
    {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::OperationInFlight,
        ));
    }
    // A live Pod without its management Service is incomplete routing, not a
    // missing row: repair belongs to admitted creation/resume, which owns the
    // durable operation and runtime lease.
    if state
        .runtime()
        .get_container_info_by_identifier(app_id, &ServiceType::UserappBuilder)
        .await?
        .is_none()
    {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::NotObserved,
        ));
    }
    let verified = ContainerBasicInfo {
        container_id: observed.container_id.clone(),
        container_name: observed.container_name.clone(),
        container_ip: observed.container_ip.clone(),
        internal_port: AGENT_FILE_SERVER_PORT,
        external_port: 0,
        project_id: app_id.into(),
        status: String::from(observed.status.clone()),
        created_at: observed.created_at,
        service_url: format!(
            "http://{}:{}",
            observed.container_ip, AGENT_FILE_SERVER_PORT
        ),
        workload_uid: observed.workload_uid.clone(),
    };
    // Shared SQL registry: only a succeeded, identity-bound Ensure receipt may
    // publish the registration through the ordinary transactional chain. When
    // that receipt is missing the gate is NOT loosened: the recovery admits a
    // new, real registration-confirmation intent through the original adoption
    // coordinator (below), which repeats the same verification and publishes
    // with its OWN receipt.
    let mut confirmation_required = false;
    match creation::repair_live_registration(state, app_id, app_id, &verified).await {
        Ok(true) => {
            return Ok(BuilderRegistrationRecovery::Completed(verified));
        }
        Ok(false) => {}
        Err(error) if error.is::<creation::BuilderRegistrationConfirmationRequired>() => {
            confirmation_required = true;
        }
        Err(error) => return Err(error),
    }
    // Ordinary backend: authority is the binding of the CURRENT lifecycle to
    // the observed controller — the same evidence that protects this workload.
    if !bound_to_current_lifecycle(state, app_id, &app.lifecycle_id, &verified).await? {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::BindingMissing,
        ));
    }
    // Original-volume continuity, in two independent layers:
    //
    // 1. a non-vacuous live witness — at least one claimed workspace volume
    //    exists, is not deleting, carries a UID, and is stamped with THIS
    //    application and lifecycle (the runtime validates the annotations);
    // 2. whenever the control store HAS a recorded discovery witness for this
    //    app/lifecycle, the live volumes must match it exactly — PVC UID
    //    equality, not deleting, and an unchanged mount/claim set
    //    (`verify_recovered_volumes` + `verify_recovered_mounts`, the same
    //    verification the ensure path runs before reusing a recovered workspace).
    //
    // Layer 2 is what catches a same-named PVC recreated with a new UID. The
    // witness only exists for apps whose identity was restored from retained
    // storage, so its absence must not block layer 1 — but when present it is
    // authoritative history and is never rewritten.
    if !workspace_volumes_verified(state, app_id, &app.lifecycle_id, &verified_target).await? {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::VolumeUnverified,
        ));
    }
    if confirmation_required {
        // 证据充分且当前意图允许：由原协调者受理“仅登记确认”并完成发布
        //（任务书 §4.6）。调用方已持有 workload 准入与本地生命周期锁。
        return submit_registration_confirmation(state, app_id, &app, &verified).await;
    }
    // A first-ever registration starts its own idle window; re-observing an
    // already registered instance would keep its historical clock instead
    // (same rule as `register_builder`).
    let mut project = state
        .projects
        .get(app_id)
        .map(|old| (*old).clone())
        .unwrap_or_else(|| ProjectAndContainerInfo::new(app_id.to_owned()));
    if project
        .container_info()
        .is_none_or(|previous| previous.container_id != verified.container_id)
    {
        project.update_activity();
    }
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_container(Some(verified.clone()));
    let outcome = state
        .projects
        .insert_project_registration_durable(app_id.into(), Arc::new(project))
        .await?;
    Ok(if outcome == PersistenceWriteOutcome::Committed {
        BuilderRegistrationRecovery::Completed(verified)
    } else {
        // Superseded (a concurrent writer won) or Deferred (a Project Stop is
        // awaiting durable confirmation): the outcome is unknown, never success.
        BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::WriteNotConfirmed)
    })
}

/// 证据充分（身份 + 绑定 + 卷 + 计算意图允许）时，经原 Adopt 协调者受理
/// 一个新的“仅登记确认”意图并完成发布（任务书 §4.5/§4.6）。确定性
/// request id 绑定物理 Pod：重放与并发副本收敛到同一操作；Pending 由既有
/// 恢复扫描 resume；Failed 不自动重试（原因可见，证据变化或显式人工确认
/// 再进）。调用方持有 workload 准入与本地生命周期锁，与 `execute` 同序。
async fn submit_registration_confirmation(
    state: &AppState,
    app_id: &str,
    app: &shared_types::UserAppLifecycleRecord,
    verified: &ContainerBasicInfo,
) -> Result<BuilderRegistrationRecovery> {
    // 计算意图围栏：desired_stopped 或在途 compute 控制 = “当前意图不允许”。
    // 已停计算只修预期停态（F2 的范围），不为补登记而启动或改动。
    let compute = state
        .userapp_store
        .read_compute_status(
            app_id,
            &app.lifecycle_id,
            shared_types::UserAppOperationScope::Dev,
        )
        .await?;
    if compute.desired_stopped
        || compute
            .operation
            .as_ref()
            .is_some_and(|operation| !operation.state.is_terminal())
    {
        return Ok(BuilderRegistrationRecovery::Unconfirmed(
            RegistrationGap::OperationInFlight,
        ));
    }
    let request = shared_types::AdoptBuilderRequest {
        lifecycle_id: app.lifecycle_id.clone(),
        request_id: format!("registration-recovery-{}", verified.container_id),
        expected_container_id: verified.container_id.clone(),
    };
    let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&request)?);
    let admitted = state
        .userapp_store
        .admit_with_input(
            &shared_types::UserAppAdmission {
                app_id: app_id.to_owned(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                request_id: Some(request.request_id.clone()),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_fingerprint: input.digest(),
                kind: shared_types::UserAppOperationKind::AdoptBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await?;
    match admitted {
        shared_types::UserAppAdmissionOutcome::Accepted(record) => {
            let operation_id = record.operation_id.clone();
            if let Err(error) = adoption::run_admitted(state, record).await {
                // 受理后的确定性拒绝已由协调者记录为 Failed（含具体原因）；
                // 提交期不确定结果（RecoveryRequired）保持未知向上传播，
                // 由扫描按 unknown 处理，不伪装成已判定缺口。
                let current = state
                    .userapp_store
                    .get_operation(app_id, &operation_id)
                    .await?;
                if matches!(&current, Some(operation)
                    if operation.state == shared_types::UserAppOperationState::Failed)
                {
                    tracing::warn!(
                        app_id,
                        operation_id = %operation_id,
                        error = %shared_types::sanitize_error_text(&format!("{error:#}")),
                        stage = "builder_registration_confirmation",
                        "registration confirmation was rejected; resource retained"
                    );
                    return Ok(BuilderRegistrationRecovery::Unconfirmed(
                        RegistrationGap::ConfirmationRejected,
                    ));
                }
                return Err(error);
            }
        }
        shared_types::UserAppAdmissionOutcome::Existing(record)
            if record.state == shared_types::UserAppOperationState::Succeeded =>
        {
            // 另一副本（或上一轮）已用同一证据完成确认：不重复执行，直接核对发布。
        }
        shared_types::UserAppAdmissionOutcome::Existing(record) => {
            if record.state == shared_types::UserAppOperationState::Failed {
                tracing::warn!(
                    app_id,
                    operation_id = %record.operation_id,
                    error = record.error_message.as_deref().unwrap_or("unknown"),
                    stage = "builder_registration_confirmation",
                    "registration confirmation was rejected; resource retained"
                );
                return Ok(BuilderRegistrationRecovery::Unconfirmed(
                    RegistrationGap::ConfirmationRejected,
                ));
            }
            // Pending/Running：既有恢复扫描会 resume（recovery.rs discover）。
            return Ok(BuilderRegistrationRecovery::Unconfirmed(
                RegistrationGap::OperationInFlight,
            ));
        }
    }
    // 发布核对以注册表为准（不是本副本内存插入的自证）。
    let published = state
        .projects
        .get(app_id)
        .and_then(|project| project.container_info())
        .is_some_and(|container| container.container_id == verified.container_id);
    Ok(if published {
        BuilderRegistrationRecovery::Completed(verified.clone())
    } else {
        BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::WriteNotConfirmed)
    })
}

/// 同一当前生命周期对该 controller 的有效绑定。缺绑定、跨应用或跨代次的绑定
/// 都不构成补齐登记的依据。
///
/// 绑定按 **workload UID**（StatefulSet）检索：这正是现场形态（controller 重建
/// Pod 后新代次从未落登记）。Docker 容器没有 workload UID，其物理身份是容器 ID，
/// 本批不据此补齐——缺证据即保留，交由显式受控恢复入口处理。
async fn bound_to_current_lifecycle(
    state: &AppState,
    app_id: &str,
    lifecycle_id: &str,
    verified: &ContainerBasicInfo,
) -> Result<bool> {
    let Some(uid) = verified
        .workload_uid
        .as_deref()
        .filter(|uid| !uid.is_empty())
    else {
        return Ok(false);
    };
    Ok(state
        .userapp_store
        .get_resource_binding(&ServiceType::UserappBuilder, uid)
        .await?
        .is_some_and(|binding| {
            binding.app_id == app_id
                && binding.lifecycle_id == lifecycle_id
                && binding.service_type == ServiceType::UserappBuilder
        }))
}

async fn workspace_volumes_verified(
    state: &AppState,
    app_id: &str,
    lifecycle_id: &str,
    target: &shared_types::BuilderControlTarget,
) -> Result<bool> {
    let volumes = state
        .runtime()
        .capture_builder_compute_volumes(target)
        .await?;
    if volumes.is_empty() {
        return Ok(false);
    }
    if !volumes.iter().all(|volume| {
        volume.kind == shared_types::AppResourceKind::PersistentVolumeClaim
            && !volume.name.is_empty()
            && !volume.uid.is_empty()
    }) {
        return Ok(false);
    }
    // 记录在案的原卷期望存在时，逐项核对 live 卷（UID 相等、未删除、挂载集未变）。
    let recorded = state
        .userapp_store
        .get_recovery_storage_witness(app_id, lifecycle_id)
        .await?
        .is_some_and(|witness| !witness.dev_volumes.is_empty());
    if !recorded {
        return Ok(true);
    }
    match state
        .app_service
        .verify_recovered_storage(app_id, shared_types::UserAppOperationScope::Dev)
        .await
    {
        Ok(()) => Ok(true),
        // Conflict rejects this observation, but may describe a PVC mismatch,
        // lifecycle change or a changed resource view. Preserve its actual
        // cause at the default log level instead of claiming the PVC changed.
        // Read failures/timeouts still propagate as unknown observations.
        Err(error @ app_manager::AppOperationError::Conflict(_)) => {
            tracing::warn!(
                app_id,
                stage = "builder_registration_volume_verification",
                error = %shared_types::sanitize_error_text(&format!("{error:#}")),
                "builder workspace verification rejected; registration retained"
            );
            Ok(false)
        }
        Err(error) => Err(error).context("verify recorded builder workspace volumes"),
    }
}

#[cfg(all(test, feature = "rcoder-pg"))]
mod pg_tests {
    use super::*;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerRuntimeResult, RuntimeContainerInfo,
        UserAppDeploymentRuntime, WorkspaceRuntime,
    };

    const DSN_ENV: &str = "RCODER_PG_TEST_DSN";

    /// 真实 PG 下的受控恢复：登记必须落库，并且**另一个独立连接**能读到
    /// （跨副本可见性），而不是仅本副本内存插入。
    #[tokio::test]
    async fn recovered_registration_is_durable_and_visible_to_another_replica() {
        let dsn = std::env::var(DSN_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .expect("explicit PG contract requires an isolated database");
        let app_id = format!("builder-recover-{}", uuid::Uuid::new_v4().simple());
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("recovery.db"),
            )
            .await
            .unwrap(),
        );
        let runtime = Arc::new(Runtime {
            app_id: app_id.clone(),
            pod_uid: format!("pod-{}", uuid::Uuid::new_v4().simple()),
            workload_uid: format!("sts-{}", uuid::Uuid::new_v4().simple()),
        });
        let mut state =
            (*crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None)
                .await)
                .clone();
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        };
        let backend = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(Arc::new(
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap(),
        )));
        state.projects = backend;
        // 独立 UserApp 控制库（与 Agent 注册表不同库）：`requires_durable_confirmation`
        // 为 false，权威来自当前生命周期绑定，登记仍必须落库并对另一副本可见。
        state.config.userapp_storage.postgres = Some(rcoder_storage::config::PostgresConfig {
            url: Some(format!(
                "postgres://testpg:testpg@127.0.0.1:32768/idle_control_{}",
                uuid::Uuid::new_v4().simple()
            )),
            ..Default::default()
        });
        let state = Arc::new(state);

        let lifecycle = state.userapp_store.ensure_identity(&app_id).await.unwrap();
        // 当前生命周期绑定（今天保护该 workload 的同一证据）。
        let input = shared_types::UserAppExecutionInput::new(
            serde_json::to_string(&shared_types::AdoptBuilderRequest {
                lifecycle_id: lifecycle.lifecycle_id.clone(),
                request_id: "recovery-request".into(),
                expected_container_id: runtime.pod_uid.clone(),
            })
            .unwrap(),
        );
        let admitted = match state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: app_id.clone(),
                    lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                    request_id: Some("recovery-request".into()),
                    operation_id: "recovery-binding".into(),
                    request_fingerprint: input.digest(),
                    kind: shared_types::UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await
            .unwrap()
        {
            shared_types::UserAppAdmissionOutcome::Accepted(record) => record,
            other => panic!("fresh fixture admission: {other:?}"),
        };
        let running = state
            .userapp_store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: admitted.app_id.clone(),
                lifecycle_id: admitted.lifecycle_id.clone(),
                operation_id: admitted.operation_id.clone(),
                expected_revision: admitted.revision,
                executor_id: "fixture".into(),
                state: shared_types::UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        state
            .userapp_store
            .commit_resource_binding(
                &shared_types::UserAppResourceBinding {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: runtime.workload_uid.clone(),
                    adopted_by_operation: running.operation_id.clone(),
                },
                &shared_types::UserAppOperationProgress {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    expected_revision: running.revision,
                    executor_id: "fixture".into(),
                    state: shared_types::UserAppOperationState::Succeeded,
                    step: "adopted".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                },
            )
            .await
            .unwrap();

        let recovered = match recover_missing_builder_registration(&state, &app_id)
            .await
            .unwrap()
        {
            BuilderRegistrationRecovery::Completed(verified) => verified,
            BuilderRegistrationRecovery::Unconfirmed(gap) => {
                panic!("current-lifecycle binding must complete the registration: {gap:?}")
            }
        };
        assert_eq!(recovered.container_id, runtime.pod_uid);
        assert_eq!(
            recovered.workload_uid.as_deref(),
            Some(runtime.workload_uid.as_str())
        );

        // 另一个副本的独立连接必须能读到同一登记（不是仅内存插入）。
        let peer =
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap();
        let peer_view = peer.get(&app_id).expect("peer replica must read the row");
        let peer_container = peer_view.container_info().expect("builder container");
        assert_eq!(peer_container.container_id, runtime.pod_uid);
        assert_eq!(peer_view.service_type(), Some(ServiceType::UserappBuilder));
    }

    /// 共享 SQL 注册表（projects 与控制库**同库**、`userapp_storage.postgres=None`）
    /// 下，缺回执但证据充分（当前生命周期绑定 + 卷见证）时，恢复链必须自动
    /// 受理“仅登记确认”工作（任务书 §4.6）：经原 Adopt 协调者 admission →
    /// runtime 核验/租约 → 自有回执 → 原子发布；而不是停在缺口告警。
    #[tokio::test]
    async fn shared_registry_recovery_submits_registration_confirmation() {
        let (state, runtime, app, dsn) = shared_registration_fixture().await;
        let recovered = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .unwrap();
        let completed = match recovered {
            BuilderRegistrationRecovery::Completed(verified) => verified,
            BuilderRegistrationRecovery::Unconfirmed(gap) => {
                panic!(
                    "evidence-sufficient recovery must submit a registration confirmation: {gap:?}"
                )
            }
        };
        assert_eq!(completed.container_id, runtime.pod_uid);

        // 登记对另一独立连接可见（跨副本，不是仅本副本内存插入）。
        let peer = rcoder_storage::pg::PgStore::connect(
            &rcoder_storage::config::PostgresConfig {
                url: Some(dsn.clone()),
                ..Default::default()
            },
            "test-ns".into(),
            "cluster.local".into(),
        )
        .await
        .unwrap();
        let registered = peer
            .get(&app.app_id)
            .expect("peer replica must read the row");
        assert_eq!(
            registered.container_info().unwrap().container_id,
            runtime.pod_uid
        );
        assert_eq!(registered.service_type(), Some(ServiceType::UserappBuilder));

        // 受理的“仅登记确认”操作真实存在且 Succeeded（自有回执，非复制旧结果）。
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        let rows = toasty::sql::query(
            "SELECT state FROM userapp_operations WHERE app_id=$1 AND kind='adopt_builder'",
        )
        .bind(&app.app_id)
        .exec(&mut admin)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "binding fixture op + the submitted registration confirmation"
        );
    }

    /// 共享形态夹具：控制库与 projects 同一 PG database（每用例独库），
    /// `userapp_storage.postgres=None`，当前生命周期绑定就绪、无任何回执。
    async fn shared_registration_fixture() -> (
        Arc<AppState>,
        Arc<Runtime>,
        shared_types::UserAppLifecycleRecord,
        String,
    ) {
        let base = std::env::var(DSN_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .expect("explicit PG contract requires an isolated database");
        let database = format!("f1t{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
        let mut admin = toasty::Db::builder().connect(&base).await.unwrap();
        toasty::sql::statement(format!("CREATE DATABASE {database}"))
            .exec(&mut admin)
            .await
            .unwrap();
        let server = base
            .trim_end_matches('/')
            .rsplit_once('/')
            .expect("DSN with a database path")
            .0
            .to_string();
        let dsn = format!("{server}/{database}");
        let app_id = format!("builder-shared-{}", uuid::Uuid::new_v4().simple());
        let runtime = Arc::new(Runtime {
            app_id: app_id.clone(),
            pod_uid: format!("pod-{}", uuid::Uuid::new_v4().simple()),
            workload_uid: format!("sts-{}", uuid::Uuid::new_v4().simple()),
        });
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(
                &rcoder_storage::config::PostgresConfig {
                    url: Some(dsn.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
        let mut state =
            (*crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None)
                .await)
                .clone();
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        };
        state.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(Arc::new(
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap(),
        )));
        state.config.userapp_storage.postgres = None;
        let state = Arc::new(state);

        let app = state.userapp_store.ensure_identity(&app_id).await.unwrap();
        let request = shared_types::AdoptBuilderRequest {
            lifecycle_id: app.lifecycle_id.clone(),
            request_id: "binding-request".into(),
            expected_container_id: runtime.pod_uid.clone(),
        };
        let input =
            shared_types::UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
        let admitted = state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    request_id: Some("binding-request".into()),
                    operation_id: format!("binding-{}", uuid::Uuid::new_v4().simple()),
                    request_fingerprint: input.digest(),
                    kind: shared_types::UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await
            .unwrap();
        let running = match admitted {
            shared_types::UserAppAdmissionOutcome::Accepted(record) => state
                .userapp_store
                .advance(&shared_types::UserAppOperationProgress {
                    app_id: record.app_id.clone(),
                    lifecycle_id: record.lifecycle_id.clone(),
                    operation_id: record.operation_id.clone(),
                    expected_revision: record.revision,
                    executor_id: "fixture".into(),
                    state: shared_types::UserAppOperationState::Running,
                    step: "claimed".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap(),
            other => panic!("fresh fixture admission: {other:?}"),
        };
        state
            .userapp_store
            .commit_resource_binding(
                &shared_types::UserAppResourceBinding {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: runtime.workload_uid.clone(),
                    adopted_by_operation: running.operation_id.clone(),
                },
                &shared_types::UserAppOperationProgress {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    expected_revision: running.revision,
                    executor_id: "fixture".into(),
                    state: shared_types::UserAppOperationState::Succeeded,
                    step: "adopted".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                },
            )
            .await
            .unwrap();
        (state, runtime, app, dsn)
    }

    /// 同一确定性 request 的确认在途（Pending）时：本轮不重复受理、不写登记，
    /// 报 operation_in_flight（既有恢复扫描负责 resume）。
    #[tokio::test]
    async fn shared_registry_recovery_defers_to_pending_confirmation() {
        let (state, runtime, app, dsn) = shared_registration_fixture().await;
        // 预置与恢复链相同确定性 request id 的 Pending 确认操作。
        let request = shared_types::AdoptBuilderRequest {
            lifecycle_id: app.lifecycle_id.clone(),
            request_id: format!("registration-recovery-{}", runtime.pod_uid),
            expected_container_id: runtime.pod_uid.clone(),
        };
        let input =
            shared_types::UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
        state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: app.app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    request_id: Some(request.request_id.clone()),
                    operation_id: format!("pending-{}", uuid::Uuid::new_v4().simple()),
                    request_fingerprint: input.digest(),
                    kind: shared_types::UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await
            .unwrap();

        let recovered = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .unwrap();
        assert_eq!(
            recovered,
            BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::OperationInFlight),
            "a pending confirmation owns the registration decision"
        );
        assert!(
            state
                .projects
                .get(&app.app_id)
                .is_none_or(|project| project.container_info().is_none())
        );
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        let rows = toasty::sql::query(
            "SELECT state FROM userapp_operations WHERE app_id=$1 AND kind='adopt_builder'",
        )
        .bind(&app.app_id)
        .exec(&mut admin)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "binding fixture + the pending confirmation; no duplicate admission"
        );
    }

    /// 已被拒绝的确认（Failed，同一确定性 request）：不自动重试、不写登记，
    /// 报 confirmation_rejected——证据变化（换代）或显式人工确认再进。
    #[tokio::test]
    async fn rejected_registration_confirmation_is_reported_not_retried() {
        let (state, runtime, app, _dsn) = shared_registration_fixture().await;
        let request = shared_types::AdoptBuilderRequest {
            lifecycle_id: app.lifecycle_id.clone(),
            request_id: format!("registration-recovery-{}", runtime.pod_uid),
            expected_container_id: runtime.pod_uid.clone(),
        };
        let input =
            shared_types::UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
        let admitted = state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: app.app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    request_id: Some(request.request_id.clone()),
                    operation_id: format!("failed-{}", uuid::Uuid::new_v4().simple()),
                    request_fingerprint: input.digest(),
                    kind: shared_types::UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await
            .unwrap();
        let running = match admitted {
            shared_types::UserAppAdmissionOutcome::Accepted(record) => state
                .userapp_store
                .advance(&shared_types::UserAppOperationProgress {
                    app_id: record.app_id.clone(),
                    lifecycle_id: record.lifecycle_id.clone(),
                    operation_id: record.operation_id.clone(),
                    expected_revision: record.revision,
                    executor_id: "fixture".into(),
                    state: shared_types::UserAppOperationState::Running,
                    step: "claimed".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap(),
            other => panic!("fresh fixture admission: {other:?}"),
        };
        state
            .userapp_store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                expected_revision: running.revision,
                executor_id: "fixture".into(),
                state: shared_types::UserAppOperationState::Failed,
                step: "adoption_failed".into(),
                checkpoint: running.checkpoint.clone(),
                error_code: Some("ERR_BACKEND_ERROR".into()),
                error_message: Some("rejected for the test".into()),
            })
            .await
            .unwrap();

        let recovered = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .unwrap();
        assert_eq!(
            recovered,
            BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::ConfirmationRejected),
            "a rejected confirmation must not be retried blindly"
        );
        assert!(
            state
                .projects
                .get(&app.app_id)
                .is_none_or(|project| project.container_info().is_none()),
            "no registration is written for a rejected confirmation"
        );
    }

    /// 计算意图不允许（在途 Stop 控制）时不受理确认：已停/停机中的计算只按
    /// 预期停态处理（任务书 §4.7），不为补登记而动它。
    #[tokio::test]
    async fn in_flight_compute_stop_defers_registration_confirmation() {
        let (state, runtime, app, _dsn) = shared_registration_fixture().await;
        state
            .userapp_store
            .admit_compute_control(&shared_types::ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: shared_types::UserAppOperationScope::Dev,
                action: shared_types::ComputeControlAction::Stop,
                operation_id: format!("stop-{}", uuid::Uuid::new_v4().simple()),
                request_id: "stop-request".into(),
                request_fingerprint: "ab".repeat(32),
                restart_image_roll: false,
                builder_startup_mode: Default::default(),
            })
            .await
            .unwrap();

        let recovered = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .unwrap();
        assert_eq!(
            recovered,
            BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::OperationInFlight),
            "an in-flight compute stop owns the registration decision"
        );
        assert!(
            state
                .projects
                .get(&app.app_id)
                .is_none_or(|project| project.container_info().is_none())
        );
        drop(runtime);
    }

    /// 注册表已有同名 containers 行（悬空引用）时：确定性拒绝带具体原因
    /// 传播、不写登记；后续轮次按同一确定性 request 有界收敛，不堆操作行。
    #[tokio::test]
    async fn dangling_registry_row_rejects_confirmation_deterministically() {
        let (state, runtime, app, dsn) = shared_registration_fixture().await;
        // 预插入悬空 containers 行：命名与运行时观察的 workload 一致。
        let workload_name = runtime.app_id.clone();
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        toasty::sql::statement(
            "INSERT INTO containers (container_name, container_generation, container_id, logical_id, service_type, container_ip, internal_port, external_port, status, service_url, last_activity_at_us, created_at_us, row_revision) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .bind(&workload_name)
        .bind(format!("dangling-gen-{}", uuid::Uuid::new_v4().simple()))
        .bind("dangling-pod")
        .bind(&app.app_id)
        .bind(ServiceType::UserappBuilder.to_string())
        .bind("10.42.0.1")
        .bind(60000_i64)
        .bind(0_i64)
        .bind("running")
        .bind("")
        .bind(chrono::Utc::now().timestamp_micros())
        .bind(chrono::Utc::now().timestamp_micros())
        .bind(1_i64)
        .exec(&mut admin)
        .await
        .unwrap();

        // 首轮：确定性 SQL 拒绝带具体原因向上传播（提交期错误按不确定语义
        // 保守处理——这是既有 adoption 契约），登记不写、资源保留。
        let error = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .expect_err("a dangling registry row rejects the confirmation deterministically");
        assert!(
            format!("{error:#}").contains("Builder registry is not missing"),
            "the rejection carries its concrete reason: {error:#}"
        );
        assert!(
            state
                .projects
                .get(&app.app_id)
                .is_none_or(|project| project.container_info().is_none())
        );
        // 次轮：确定性 request id 命中同一非终态操作——有界 operation_in_flight，
        // 不再堆叠操作行，也不写登记。
        let second = recover_missing_builder_registration(&state, &app.app_id)
            .await
            .unwrap();
        assert_eq!(
            second,
            BuilderRegistrationRecovery::Unconfirmed(RegistrationGap::OperationInFlight),
            "a non-terminal confirmation bounds later rounds"
        );
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        let rows = toasty::sql::query(
            "SELECT state FROM userapp_operations WHERE app_id=$1 AND kind='adopt_builder'",
        )
        .bind(&app.app_id)
        .exec(&mut admin)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "binding fixture + exactly one confirmation op"
        );
    }

    struct Runtime {
        app_id: String,
        pod_uid: String,
        workload_uid: String,
    }

    #[async_trait::async_trait]
    impl AgentContainerRuntime for Runtime {
        fn agent_workload_name(
            &self,
            identifier: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<String> {
            Ok(identifier.into())
        }
        fn agent_workload_namespace(&self) -> String {
            "test-ns".into()
        }
        async fn find_container(
            &self,
            _: &str,
            family: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            if *family != ServiceType::UserappBuilder {
                return Ok(None);
            }
            Ok(Some(RuntimeContainerInfo {
                container_id: self.pod_uid.clone(),
                container_name: self.app_id.clone(),
                container_ip: "10.42.4.207".into(),
                status: container_runtime_api::ContainerRuntimeStatus::Running,
                created_at: chrono::Utc::now(),
                env_vars: None,
                service_type: Some(ServiceType::UserappBuilder),
                project_id: Some(self.app_id.clone()),
                user_id: None,
                pod_id: None,
                app_id: Some(self.app_id.clone()),
                workload_uid: Some(self.workload_uid.clone()),
            }))
        }
        async fn inspect_builder_candidate(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn capture_builder_control(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn capture_bound_builder_control(
            &self,
            context: &shared_types::UserAppExecutionContext,
            _binding: Option<&shared_types::UserAppResourceBinding>,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn capture_builder_compute_volumes(
            &self,
            _target: &shared_types::BuilderControlTarget,
        ) -> ContainerRuntimeResult<Vec<shared_types::AppResourceIdentity>> {
            Ok(vec![shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::PersistentVolumeClaim,
                name: format!("{}-workspace", self.app_id),
                uid: format!("pvc-{}", self.pod_uid),
                resource_version: None,
            }])
        }
        async fn acquire_builder_operation(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
            struct NoopLease;
            #[async_trait::async_trait]
            impl shared_types::AppOperationLease for NoopLease {
                async fn release(self: Box<Self>) -> Result<(), String> {
                    Ok(())
                }
            }
            Ok(Box::new(NoopLease))
        }
        async fn capture_builder_adoption(
            &self,
            context: &shared_types::UserAppExecutionContext,
            expected_container_id: &str,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            let mut target = self.target(context)?;
            let pod = target.pod.as_ref().ok_or_else(|| {
                container_runtime_api::ContainerRuntimeError::K8sError(
                    "no builder pod observed".into(),
                )
            })?;
            if pod.uid != expected_container_id {
                return Err(container_runtime_api::ContainerRuntimeError::Conflict(
                    "expected physical builder differs from the live pod".into(),
                ));
            }
            target.resource_binding = None;
            Ok(target)
        }
        async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
            panic!("registration recovery must not stop compute")
        }
        async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
            Ok(true)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            panic!("registration recovery must not delete resources")
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn sync_states(
            &self,
        ) -> ContainerRuntimeResult<(u32, Vec<container_runtime_api::RemovedContainerInfo>)>
        {
            Ok((0, vec![]))
        }
        async fn create_container(
            &self,
            _: container_runtime_api::ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            panic!("registration recovery must not create compute")
        }
        async fn get_container_info(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
    }

    impl Runtime {
        fn target(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            Ok(shared_types::BuilderControlTarget {
                context: context.clone(),
                resource_binding: None,
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::StatefulSet,
                    name: self.app_id.clone(),
                    uid: self.workload_uid.clone(),
                    resource_version: Some("1".into()),
                }),
                pod: Some(shared_types::BuilderPodIdentity {
                    name: format!("{}-0", self.app_id),
                    uid: self.pod_uid.clone(),
                    resource_version: "2".into(),
                }),
                restart_image: None,
                restart_runtime_workspace: None,
                builder_startup_mode: Default::default(),
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceRuntime for Runtime {}
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn list_deployments(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
            Ok(vec![])
        }
    }

    /// 缺证据（无当前生命周期绑定）时不得写登记——真实 PG 下同样保持保留。
    #[tokio::test]
    async fn recovered_registration_requires_current_lifecycle_binding() {
        let dsn = std::env::var(DSN_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .expect("explicit PG contract requires an isolated database");
        let app_id = format!("builder-nobind-{}", uuid::Uuid::new_v4().simple());
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("nobind.db"),
            )
            .await
            .unwrap(),
        );
        let workload_uid = format!("sts-{}", uuid::Uuid::new_v4().simple());
        let runtime = Arc::new(Runtime {
            app_id: app_id.clone(),
            pod_uid: format!("pod-{}", uuid::Uuid::new_v4().simple()),
            workload_uid,
        });
        let mut state = (*crate::userapp_builder::builder_runtime_test_state(runtime, store, None)
            .await)
            .clone();
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn),
            ..Default::default()
        };
        state.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(Arc::new(
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap(),
        )));
        let state = Arc::new(state);
        state.userapp_store.ensure_identity(&app_id).await.unwrap();
        assert!(
            matches!(
                recover_missing_builder_registration(&state, &app_id)
                    .await
                    .unwrap(),
                BuilderRegistrationRecovery::Unconfirmed(_)
            ),
            "no current-lifecycle binding must not complete a registration"
        );
    }
}

#[cfg(test)]
mod gap_tests {
    use super::*;

    /// `ALL` 是聚合计数的唯一来源：必须无重复原因串，否则有界告警会把两个不同
    /// 缺口显示成同一个。`as_str` 的穷尽 match 由编译器保证覆盖每个变体。
    #[test]
    fn gap_reasons_are_distinct_and_bounded() {
        let mut seen = std::collections::HashSet::new();
        for gap in RegistrationGap::ALL {
            assert!(
                seen.insert(gap.as_str()),
                "duplicate registration gap reason: {}",
                gap.as_str()
            );
            assert!(!gap.as_str().is_empty());
        }
        assert_eq!(seen.len(), RegistrationGap::ALL.len());
    }
}

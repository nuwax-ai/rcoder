//! Durable compute-only builder controls; HTTP observers do not own execution.
use super::dev_cleanup::BuilderOperation;
use crate::app_state::AppState;
use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt as _;
use sha2::{Digest as _, Sha256};
use shared_types::{
    BuilderControlError, BuilderControlResult, UserAppAdmission, UserAppAdmissionOutcome,
    UserAppControlCommand, UserAppControlRequest, UserAppExecutionContext, UserAppLifecycleState,
    UserAppOperationKind, UserAppOperationProgress, UserAppOperationRecord, UserAppOperationState,
    UserAppStoreError,
};

pub async fn execute(
    state: &AppState,
    app_id: &str,
    mut request: UserAppControlRequest,
    restart: bool,
) -> Result<BuilderControlResult> {
    shared_types::validate_identifier(app_id, "app_id").map_err(|error| anyhow!(error))?;
    request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
    let owned = state.clone();
    let flight = state.userapp_op_flight.guard()?;
    let app_id = app_id.to_owned();
    tokio::spawn(async move {
        // R02：在途协调任务门闸——关机时等待本任务收束后再关闭存储
        let _flight = flight;
        let _local = super::lifecycle::acquire(&app_id).await;
        let app = owned
            .userapp_store
            .get_application(&app_id)
            .await?
            .ok_or(UserAppStoreError::NotFound)?;
        if app.state != UserAppLifecycleState::Active
            || request
                .lifecycle_id
                .as_ref()
                .is_some_and(|expected| expected != &app.lifecycle_id)
        {
            return Err(UserAppStoreError::LifecycleConflict.into());
        }
        // 应用共享：物理资源定位/锁/注册按纯 app_id（与受理同一键）。
        let instance = app_id.clone();
        let command = if restart {
            UserAppControlCommand::RestartBuilder
        } else {
            UserAppControlCommand::StopBuilder
        };
        let fingerprint = Sha256::digest(shared_types::encode_userapp_intent(&(
            command.clone(),
            &request,
        ))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
        let record = match owned
            .userapp_store
            .admit(&UserAppAdmission {
                runtime_policy_on_success: None,
                metadata: None,
                command: Some(command),
                kind: if restart {
                    UserAppOperationKind::RestartBuilder
                } else {
                    UserAppOperationKind::StopBuilder
                },
                app_id: app_id.clone(),
                lifecycle_id: request.lifecycle_id,
                request_id: request.request_id,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_fingerprint: fingerprint,
            })
            .await?
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Succeeded =>
            {
                return serde_json::from_value(record.checkpoint["result"].clone())
                    .context("Decode completed builder control");
            }
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Pending =>
            {
                record
            }
            UserAppAdmissionOutcome::Existing(record) => {
                return Err(UserAppStoreError::OperationInProgress(record.blocker()).into());
            }
        };
        execute_pending(&owned, record, &instance, restart).await
    })
    .await
    .context("Builder control observer interrupted")?
}

async fn advance(
    state: &AppState,
    record: &mut UserAppOperationRecord,
    executor: &str,
    status: UserAppOperationState,
    step: &str,
    checkpoint: serde_json::Value,
    error: Option<String>,
) -> Result<()> {
    *record = state
        .userapp_store
        .advance(&UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: executor.into(),
            state: status,
            step: step.into(),
            checkpoint,
            error_code: error
                .as_ref()
                .map(|_| shared_types::error_codes::ERR_BACKEND_ERROR.into()),
            error_message: error,
        })
        .await?;
    Ok(())
}

async fn execute_pending(
    state: &AppState,
    mut record: UserAppOperationRecord,
    instance: &str,
    restart: bool,
) -> Result<BuilderControlResult> {
    let executor = uuid::Uuid::new_v4().to_string();
    advance(
        state,
        &mut record,
        &executor,
        UserAppOperationState::Running,
        "claimed",
        serde_json::Value::Null,
        None,
    )
    .await?;
    let context = UserAppExecutionContext {
        app_id: instance.to_string(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        executor_id: executor.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
    };
    let mut lease = None;
    let mut mutating = false;
    let operation = std::panic::AssertUnwindSafe(async {
        lease = Some(BuilderOperation::new(
            state.runtime().acquire_builder_operation(instance).await?,
        ));
        let receipt = lease
            .as_ref()
            .ok_or_else(|| anyhow!("Builder operation lease missing"))?
            .receipt()
            .map_err(|error| anyhow!(error))?;
        state
            .userapp_store
            .bind_operation_lease(&context, &receipt)
            .await?;
        let target = super::adoption::capture_bound_target(state, &context).await?;
        target.validate().map_err(|error| anyhow!(error))?;
        if target.context != context {
            return Err(anyhow!("Builder control target context mismatch"));
        }
        if restart && target.workload.is_none() {
            return Err(anyhow!("Builder does not exist; restart cannot create it"));
        }
        let target_json = serde_json::to_value(&target)?;
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Running,
            "compute_captured",
            target_json.clone(),
            None,
        )
        .await?;
        let container = if target.workload.is_some() {
            state
                .userapp_store
                .check_business_execution(&context)
                .await?;
            lease
                .as_mut()
                .ok_or_else(|| anyhow!("Builder operation lease missing"))?
                .begin_external_mutation()
                .map_err(|error| anyhow!(error))?;
            mutating = true;
            state
                .runtime()
                .apply_builder_control(&target, restart)
                .await?
        } else {
            None
        };
        if restart && container.is_none() {
            return Err(anyhow!(
                "Builder restart did not return its physical container"
            ));
        }
        let container = if let Some(info) = container {
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(
                    state.config.userapp_storage.ensure_timeout_seconds,
                );
            let info =
                super::confirm_builder_ready(state, &record.app_id, instance, info, deadline)
                    .await?;
            super::register_builder(state, instance, &info)?;
            Some(info)
        } else {
            None
        };
        let result = BuilderControlResult {
            operation_id: record.operation_id.clone(),
            was_existing: target.workload.is_some(),
            container,
        };
        let checkpoint = serde_json::json!({"target": target_json, "result": result});
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Running,
            "compute_confirmed",
            checkpoint.clone(),
            None,
        )
        .await?;
        // Registration is an observation, not authority. Do not clear an entire
        // project/session or overwrite a newer registry generation after stopping.
        crate::userapp_forward::invalidate_probe_cache(instance);
        let guard = lease
            .as_mut()
            .ok_or_else(|| anyhow!("Builder operation lease missing"))?;
        if mutating {
            guard.finish_external_mutation().await
        } else {
            guard.finish_read_only().await
        }
        .map_err(|error| anyhow!(error))?;
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Succeeded,
            "completed",
            checkpoint,
            None,
        )
        .await?;
        forget_released_lease(
            state,
            &shared_types::UserAppOperationLeaseBinding {
                context: context.clone(),
                receipt,
            },
        )
        .await;
        Ok::<_, anyhow::Error>(result)
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|_| {
        Err(anyhow!(
            "Builder control worker panicked; inspect its durable operation"
        ))
    });
    match operation {
        Ok(result) => Ok(result),
        Err(error) => {
            let rejected = error
                .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                .is_some_and(|error| {
                    matches!(
                        error,
                        container_runtime_api::ContainerRuntimeError::RequestRejected(_)
                    )
                });
            let mut release_failed = false;
            if mutating
                && rejected
                && let Some(guard) = lease.as_mut()
            {
                match guard.finish_external_mutation().await {
                    Ok(()) => mutating = false,
                    Err(release_error) => {
                        release_failed = true;
                        tracing::error!(operation_id = %record.operation_id, %release_error, "Rejected builder control lease release failed");
                    }
                }
            } else if !mutating
                && let Some(guard) = lease.as_mut()
                && let Err(release_error) = guard.finish_read_only().await
            {
                release_failed = true;
                tracing::error!(operation_id = %record.operation_id, %release_error, "Read-only builder control lease release failed");
            }
            let message = format!("{error:#}");
            let checkpoint = record.checkpoint.clone();
            let status = if mutating || release_failed {
                UserAppOperationState::RecoveryRequired
            } else {
                UserAppOperationState::Failed
            };
            // Keep the last durable evidence boundary for reconciliation. In
            // particular, a terminal SQL failure after compute confirmation must
            // not erase the fact that the runtime operation already completed.
            let failure_step = if status == UserAppOperationState::RecoveryRequired {
                record.step.clone()
            } else {
                "control_failed".to_owned()
            };
            if let Err(storage_error) = advance(
                state,
                &mut record,
                &executor,
                status,
                &failure_step,
                checkpoint,
                Some(message.clone()),
            )
            .await
            {
                tracing::error!(operation_id = %record.operation_id, %storage_error, "Record builder control failure failed");
            }
            Err(BuilderControlError {
                operation_id: record.operation_id,
                message,
            }
            .into())
        }
    }
}

async fn forget_released_lease(
    state: &AppState,
    binding: &shared_types::UserAppOperationLeaseBinding,
) {
    // Runtime release and terminal commit are already confirmed. A failure to
    // retire this cleanup record is retried by discovery, not by repeating I/O.
    if let Err(error) = state.userapp_store.forget_operation_lease(binding).await {
        tracing::warn!(%error, operation_id = %binding.context.operation_id,
            "Completed builder lease cleanup record remains pending");
    }
}

/// Recovery only finalizes a confirmed effect; it never starts/stops the
/// container again and never derives a lease owner from the application name.
pub(super) async fn reconcile_completed(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if !matches!(
        snapshot.kind,
        UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
    ) || !shared_types::userapp_operation_has_final_evidence(snapshot)
    {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        return Ok(false);
    };
    let binding = state
        .userapp_store
        .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Builder recovery lease receipt missing"))?;
    if binding.receipt.service_type() != &shared_types::ServiceType::UserappBuilder {
        return Err(anyhow!("Builder recovery lease family mismatch"));
    }
    // 恢复只按已存证据行事：lease binding context 的 app_id 槽即创建时的
    // 复合 identifier（不从纯 app_id 重派生）。
    let instance = binding.context.app_id.clone();
    let mut reserved = state
        .userapp_store
        .reserve_completed_operation(snapshot)
        .await?;
    let result = state
        .runtime()
        .release_app_operation_receipt(&binding.context, &binding.receipt)
        .await;
    // The completed checkpoint is already reserved durably: a failed release
    // is pending cleanup, never an unknown outcome. The Lease self-expires
    // within its TTL (a legacy marker is retired by the 24h sweep), and
    // terminal-lease discovery retries release+forget — re-fencing a
    // completed operation here recreates the permanent-block class.
    if let Err(error) = &result {
        tracing::warn!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            %error,
            "Completed builder lease release failed; terminal-lease discovery will retry"
        );
    }
    let checkpoint = reserved.checkpoint.clone();
    let step = reserved.step.clone();
    advance(
        state,
        &mut reserved,
        &binding.context.executor_id,
        UserAppOperationState::Succeeded,
        &step,
        checkpoint,
        None,
    )
    .await?;
    if result.is_ok() {
        forget_released_lease(state, &binding).await;
    }
    crate::userapp_forward::invalidate_probe_cache(&instance);
    Ok(true)
}

pub(super) async fn resume_pending(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> Result<bool> {
    let restart = match record.command.as_ref() {
        Some(UserAppControlCommand::StopBuilder) => false,
        Some(UserAppControlCommand::RestartBuilder) => true,
        _ => return Ok(false),
    };
    let Some(_local) = super::lifecycle::try_acquire(&record.app_id).await else {
        return Ok(false);
    };
    let current = state
        .userapp_store
        .get_operation(&record.app_id, &record.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if current.state != UserAppOperationState::Pending || current.revision != record.revision {
        return Ok(false);
    }
    let app = state
        .userapp_store
        .get_application(&record.app_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if app.lifecycle_id != record.lifecycle_id || app.state != UserAppLifecycleState::Active {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    // 应用共享：instance == 纯 app_id
    let instance = record.app_id.clone();
    execute_pending(state, current, &instance, restart).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    //! K02 回归：写后复核冲突不得当作写前拒绝——上层 control worker 必须保留
    //! mutating 保护、操作转 RecoveryRequired、租约不释放、后续写被围栏拒绝。
    //! 对照组：真正的事前拒绝（RequestRejected）记 Failed 并释放租约。
    use super::*;
    use crate::app_state::AppState;
    use crate::config::AppConfig;
    use crate::grpc::{GrpcChannelPool, SessionStreamRegistry};
    use crate::storage::{ProjectAdapter, ProjectStoreBackend};
    use agent_provisioning::AgentDownloadManager;
    use app_manager::AppActivityRegistry;
    use app_manager::config::{AppAccessMode, AppManagerConfig};
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError,
        ContainerRuntimeResult, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
    };
    use dashmap::DashMap;
    use shared_types::{ApiKeyAuthConfig, ContainerBasicInfo};
    use std::sync::{Arc, Mutex};
    use tokio::sync::broadcast;

    #[derive(Clone, Copy, PartialEq)]
    enum ApplyOutcome {
        PostWriteConflict,
        PreWriteRejected,
    }

    struct FakeLease(Arc<Mutex<LeaseState>>);
    struct LeaseState {
        releases: u32,
        mutations: u32,
    }
    #[derive(Default)]
    struct CaptureGate {
        captured: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl shared_types::AppOperationLease for FakeLease {
        fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
            Some(shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                service_type: shared_types::ServiceType::UserappBuilder,
                namespace: "test-ns".into(),
                name: "rcoder-operation-builder-testapp".into(),
                uid: "lease-uid".into(),
                resource_version: "1".into(),
                token: "op-token".into(),
            })
        }
        async fn release(self: Box<Self>) -> Result<(), String> {
            self.0.lock().expect("lease state").releases += 1;
            Ok(())
        }
    }

    struct ControlRuntime {
        gate: Option<Arc<CaptureGate>>,
        outcome: ApplyOutcome,
        lease_state: Arc<Mutex<LeaseState>>,
    }

    #[async_trait]
    impl AgentContainerRuntime for ControlRuntime {
        async fn create_container(
            &self,
            _params: ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            Err(ContainerRuntimeError::ContainerNotFound("probe".into()))
        }
        async fn get_container_info(
            &self,
            _project_id: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
        async fn find_container(
            &self,
            _identifier: &str,
            _service_type: &shared_types::ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            Ok(None)
        }
        async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn acquire_builder_operation(
            &self,
            _app_id: &str,
        ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
            Ok(Box::new(FakeLease(self.lease_state.clone())))
        }
        async fn inspect_builder_candidate(
            &self,
            context: &UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            Ok(shared_types::BuilderControlTarget {
                resource_binding: None,
                context: context.clone(),
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::StatefulSet,
                    name: "rcoder-app-builder-testapp".into(),
                    uid: "sts-uid".into(),
                    resource_version: Some("9".into()),
                }),
                pod: None,
            })
        }
        async fn capture_bound_builder_control(
            &self,
            context: &UserAppExecutionContext,
            _binding: Option<&shared_types::UserAppResourceBinding>,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            if let Some(gate) = &self.gate {
                gate.captured.notify_one();
                gate.resume.notified().await;
            }
            Ok(shared_types::BuilderControlTarget {
                resource_binding: None,
                context: context.clone(),
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::StatefulSet,
                    name: "rcoder-app-builder-testapp".into(),
                    uid: "sts-uid".into(),
                    resource_version: Some("9".into()),
                }),
                pod: None,
            })
        }
        async fn apply_builder_control(
            &self,
            _target: &shared_types::BuilderControlTarget,
            _restart: bool,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            self.lease_state.lock().expect("lease state").mutations += 1;
            match self.outcome {
                // 写已发生（patch 落盘），最终复核发现身份被替换 → K02 修复后
                // 的错误类别（docker_manager 写后复核不再映射 RequestRejected）
                ApplyOutcome::PostWriteConflict => Err(ContainerRuntimeError::Conflict(
                    "builder workload identity changed after write".into(),
                )),
                // 真正的事前拒绝（捕获/前置校验阶段）
                ApplyOutcome::PreWriteRejected => Err(ContainerRuntimeError::RequestRejected(
                    shared_types::RuntimeRequestRejection {
                        status: 409,
                        message: "captured identity stale before write".into(),
                    },
                )),
            }
        }
    }

    #[async_trait]
    impl WorkspaceRuntime for ControlRuntime {}
    #[async_trait]
    impl UserAppDeploymentRuntime for ControlRuntime {
        async fn list_deployments(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
            Ok(vec![])
        }
    }

    async fn test_state(
        outcome: ApplyOutcome,
    ) -> (Arc<AppState>, Arc<Mutex<LeaseState>>, tempfile::TempDir) {
        test_state_with_gate(outcome, None).await
    }

    async fn test_state_with_gate(
        outcome: ApplyOutcome,
        gate: Option<Arc<CaptureGate>>,
    ) -> (Arc<AppState>, Arc<Mutex<LeaseState>>, tempfile::TempDir) {
        let lease_state = Arc::new(Mutex::new(LeaseState {
            releases: 0,
            mutations: 0,
        }));
        let runtime: Arc<dyn container_runtime_api::ContainerRuntime> = Arc::new(ControlRuntime {
            gate,
            outcome,
            lease_state: lease_state.clone(),
        });
        let (adapter, _cleanup_rx) =
            ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
        let activity = Arc::new(AppActivityRegistry::new(std::time::Duration::from_secs(
            300,
        )));
        let manager_config = AppManagerConfig {
            access_mode: AppAccessMode::Docker,
            ..AppManagerConfig::default()
        };
        let metadata_dir = tempfile::tempdir().expect("metadata directory");
        let metadata_store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &metadata_dir.path().join("userapp.turso.db"),
        )
        .await
        .expect("Turso metadata store");
        let metadata_store = Arc::new(metadata_store);
        shared_types::UserAppLifecycleStore::ensure_identity(metadata_store.as_ref(), "testapp")
            .await
            .expect("identity");
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
            app_manager::service::AppService::new(
                manager_config,
                runtime.clone(),
                activity.clone(),
                None,
                metadata_store.clone(),
            )
            .await
            .expect("AppService"),
        );
        let download_dir = tempfile::tempdir().expect("download dir");
        let agent_download_manager =
            Arc::new(AgentDownloadManager::new(download_dir.path()).expect("downloads"));
        let (pod_created_tx, _) = broadcast::channel(32);
        let state = Arc::new(AppState {
            userapp_store: metadata_store.clone(),
            userapp_store_control: metadata_store,
            userapp_op_flight: Arc::new(
                crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
            ),
            userapp_recovery_handle: Arc::new(Mutex::new(None)),
            config: AppConfig::default(),
            projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
            pingora_service: None,
            grpc_pool: Arc::new(GrpcChannelPool::new()),
            session_stream_registry: Arc::new(SessionStreamRegistry::new()),
            api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
            pod_creating: Arc::new(DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "dev-rcoder".to_string(),
            container_prefix_computer: "computer-agent-runner".to_string(),
            runtime,
            cleanup_rx: Arc::new(Mutex::new(None)),
            agent_download_manager,
            app_service,
            activity,
            cluster_domain: "cluster.local".to_string(),
        });
        (state, lease_state, metadata_dir)
    }

    #[tokio::test]
    async fn priority_stop_after_capture_prevents_old_builder_mutation() {
        let gate = Arc::new(CaptureGate::default());
        let (state, lease, _dir) =
            test_state_with_gate(ApplyOutcome::PostWriteConflict, Some(gate.clone())).await;
        let owned = state.clone();
        let old = tokio::spawn(async move {
            execute(
                &owned,
                "testapp",
                UserAppControlRequest {
                    lifecycle_id: None,
                    request_id: Some("oldrestart".into()),
                },
                true,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.captured.notified())
            .await
            .expect("capture reached");
        let app = state
            .userapp_store
            .get_application("testapp")
            .await
            .unwrap()
            .unwrap();
        let stop = state
            .userapp_store
            .admit_compute_control(&shared_types::ComputeControlRequest {
                app_id: "testapp".into(),
                lifecycle_id: app.lifecycle_id,
                scope: shared_types::UserAppOperationScope::Dev,
                operation_id: "prioritystop".into(),
                request_id: "prioritystop".into(),
                request_fingerprint: "a".repeat(64),
                action: shared_types::ComputeControlAction::Stop,
            })
            .await
            .unwrap();
        gate.resume.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), old)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(lease.lock().unwrap().mutations, 0);
        assert_eq!(lease.lock().unwrap().releases, 1);
        assert_eq!(
            operation_state(&state, "oldrestart").await.state,
            UserAppOperationState::Failed
        );
        let current = state
            .userapp_store
            .get_compute_control("testapp", &stop.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, shared_types::ComputeControlState::Pending);
        assert_eq!(current.generation, stop.generation);
    }

    async fn operation_state(state: &AppState, request_id: &str) -> UserAppOperationRecord {
        state
            .userapp_store
            .get_operation_by_request("testapp", request_id)
            .await
            .expect("operation query")
            .expect("operation exists")
    }

    #[tokio::test]
    async fn post_write_conflict_keeps_protection_and_recovery_required() {
        let (state, lease_state, _dir) = test_state(ApplyOutcome::PostWriteConflict).await;
        let _error = execute(
            &state,
            "testapp",
            UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("k02-post-write".into()),
            },
            false,
        )
        .await
        .expect_err("conflict surfaces");

        let record = operation_state(&state, "k02-post-write").await;
        assert_eq!(
            record.state,
            UserAppOperationState::RecoveryRequired,
            "写后冲突必须保持未知结果保护"
        );
        assert_eq!(
            record.step, "compute_captured",
            "失败必须保留最后的持久证据边界"
        );
        assert_eq!(
            lease_state.lock().expect("lease").releases,
            0,
            "租约不得释放（操作员恢复语义）"
        );
        // 后续写被围栏拒绝：同一应用的下一个控制操作不得受理
        let blocked = execute(
            &state,
            "testapp",
            UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("k02-post-write".into()),
            },
            false,
        )
        .await;
        assert!(
            blocked.is_err(),
            "RecoveryRequired 的应用必须拒绝后续控制操作"
        );
        let record2 = operation_state(&state, "k02-post-write").await;
        assert_eq!(record2.state, UserAppOperationState::RecoveryRequired);
    }

    #[tokio::test]
    async fn pre_write_rejection_records_failed_and_releases_lease() {
        let (state, lease_state, _dir) = test_state(ApplyOutcome::PreWriteRejected).await;
        execute(
            &state,
            "testapp",
            UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("k02-pre-write".into()),
            },
            false,
        )
        .await
        .expect_err("rejection surfaces");
        let record = operation_state(&state, "k02-pre-write").await;
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "事前拒绝无副作用，直接记 Failed"
        );
        assert_eq!(record.step, "control_failed");
        assert_eq!(
            lease_state.lock().expect("lease").releases,
            1,
            "无未完成变更的拒绝必须释放租约"
        );
    }
}

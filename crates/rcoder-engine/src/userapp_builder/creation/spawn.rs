use super::*;

pub(crate) fn spawn_operation(
    state: &AppState,
    record: &UserAppOperationRecord,
    instance: &str,
    lease: OwnedMutexGuard<()>,
    flight: shared_types::FlightGuard,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let (signal, _) = watch::channel(0);
    SIGNALS
        .lock()
        .map_err(|_| anyhow!("Builder notification registry lock poisoned"))?
        .insert(record.operation_id.clone(), signal.clone());
    let worker_state = state.clone();
    let worker_record = record.clone();
    let instance = instance.to_string();
    let worker = tokio::spawn(async move {
        // R02：在途协调任务门闸——关机时等待本任务收束后再关闭存储
        // Both the observer and the actual execution retain admission. A panic
        // in the observer must not make a detached execution invisible to drain.
        let flight = Arc::new(flight);
        let execution_flight = flight.clone();
        let _flight = flight;
        let executor = uuid::Uuid::new_v4().to_string();
        let owned = worker_state.clone();
        let claimed_id = executor.clone();
        let initial = worker_record.clone();
        let execution_signal = signal.clone();
        let execution = tokio::spawn(async move {
            let _flight = execution_flight;
            let _lease = lease;
            let mut claimed = progress(
                &owned.userapp_store,
                &initial,
                &claimed_id,
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
                None,
            )
            .await?;
            let ready_deadline = Instant::now()
                + Duration::from_secs(owned.config.userapp_storage.ensure_timeout_seconds);
            let predecessor = match registration::capture_predecessor(&owned, &claimed).await {
                Ok(predecessor) => predecessor,
                Err(error) => {
                    // Only read-side identity/volume inspection has run. Record
                    // that known rejection so a fresh request may retry it.
                    progress(&owned.userapp_store, &claimed, &claimed_id,
                        UserAppOperationState::Failed, "creation_result", serde_json::Value::Null,
                        Some(format!("Inspect builder registration predecessor before creation: {error:#}"))).await?;
                    return Ok(());
                }
            };
            if let Some(predecessor) = predecessor {
                claimed = progress(&owned.userapp_store, &claimed, &claimed_id,
                    UserAppOperationState::Running, "creation_source_captured",
                    serde_json::json!({"builder_creation_predecessor": predecessor}), None).await?;
            }
            let creation = crate::userapp_builder::create_builder_inner(
                &owned,
                &claimed.app_id,
                &instance,
                shared_types::UserAppExecutionContext {
                    app_id: instance.clone(),
                    lifecycle_id: claimed.lifecycle_id.clone(),
                    operation_id: claimed.operation_id.clone(),
                    executor_id: claimed_id.clone(),
                    request_fingerprint: claimed.request_fingerprint.clone(),
                },
            );
            let creation = async {
                let result = creation.await;
                if Instant::now() >= ready_deadline {
                    // Only readonly observation after expiry; never register
                    // the instance or replay creation from this old executor.
                    match tokio::time::timeout(Duration::from_secs(30),
                        record_late_creation_result(&owned, &claimed, &result)).await
                    {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => tracing::warn!(%error,
                            operation_id = %claimed.operation_id, "Late builder evidence could not be saved"),
                        Err(error) => tracing::warn!(%error,
                            operation_id = %claimed.operation_id, "Late builder evidence observation timed out"),
                    }
                }
                result
            };
            let observed = observe_creation_budget(tokio::time::sleep_until(ready_deadline), creation, async {
                progress(
                    &owned.userapp_store,
                    &claimed,
                    &claimed_id,
                    UserAppOperationState::RecoveryRequired,
                    "creation_confirmation_timed_out",
                    claimed.checkpoint.clone(),
                    Some("Builder confirmation deadline exceeded; remote execution is still being observed".into()),
                )
                .await?;
                execution_signal.send_replace(1);
                Ok(())
            })
            .await?;
            let Some(created) = observed else {
                // A late remote result is evidence for reconciliation, never a
                // new success or a registration published by this old worker.
                return Ok(());
            };
            let result = match created {
                Ok(info) => {
                    crate::userapp_builder::confirm_builder_ready_inner(
                        &owned,
                        &claimed.app_id,
                        &instance,
                        info,
                        Some(&claimed),
                        ready_deadline,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let mut completion = claimed.clone();
            let (status, value, error) = match result {
                Ok(info) => {
                    let context = shared_types::UserAppExecutionContext {
                        // capture 侧按 app identifier 定位容器（与上方
                        // create_builder_inner 同源；应用共享后两者恒等）
                        app_id: instance.clone(),
                        lifecycle_id: claimed.lifecycle_id.clone(), operation_id: claimed.operation_id.clone(),
                        executor_id: claimed_id.clone(), request_fingerprint: claimed.request_fingerprint.clone(),
                    };
                    let evidence = registration::attach_predecessor(&owned, &claimed, shared_types::BuilderCreationEvidence {
                        registration_predecessor: None,
                        creation_lease_released: true,
                        target: crate::userapp_builder::adoption::capture_bound_target(&owned, &context).await?,
                        container: info.clone(),
                    }).await?;
                    evidence.validate_operation(&claimed).map_err(anyhow::Error::msg)?;
                    completion = progress(&owned.userapp_store, &claimed, &claimed_id,
                        UserAppOperationState::Running, "builder_ready_confirmed",
                        serde_json::to_value(&evidence)?, None).await?;
                    (
                        UserAppOperationState::Succeeded,
                        registration::completed_checkpoint(&evidence)?,
                        None,
                    )
                },
                Err(error) => {
                    let cancelled = matches!(error.downcast_ref::<container_runtime_api::ContainerRuntimeError>(),
                        Some(container_runtime_api::ContainerRuntimeError::CreationCancelled));
                    let rejected = error
                        .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                        .is_some_and(|e| {
                            matches!(
                                e,
                                container_runtime_api::ContainerRuntimeError::RequestRejected(_)
                            )
                        });
                    (
                        if rejected || cancelled {
                            UserAppOperationState::Failed
                        } else {
                            UserAppOperationState::RecoveryRequired
                        },
                        if cancelled { serde_json::json!({"creation_cancelled": true}) } else { claimed.checkpoint.clone() },
                        Some(format!("{error:#}")),
                    )
                }
            };
            progress(
                &owned.userapp_store,
                &completion,
                &claimed_id,
                status,
                "creation_result",
                value,
                error,
            )
            .await?;
            Ok::<(), anyhow::Error>(())
        })
        .await;
        if !matches!(&execution, Ok(Ok(()))) {
            // A panic or a failed checkpoint is not proof that runtime creation failed.
            if let Ok(Some(current)) = worker_state
                .userapp_store
                .get_operation(&worker_record.app_id, &worker_record.operation_id)
                .await
                && current.state == UserAppOperationState::Running
                && current.executor_id.as_deref() == Some(&executor)
                && let Err(error) = progress(
                    &worker_state.userapp_store,
                    &current,
                    &executor,
                    UserAppOperationState::RecoveryRequired,
                    if shared_types::userapp_operation_has_final_evidence(&current) {
                        &current.step
                    } else {
                        "worker_interrupted"
                    },
                    current.checkpoint.clone(),
                    Some("Builder worker did not commit its result".into()),
                )
                .await
            {
                tracing::error!(%error, operation_id = %current.operation_id, "Record interrupted builder operation failed");
            }
            tracing::error!(operation_id = %worker_record.operation_id, result = ?execution, "Builder operation requires reconciliation");
        }
        signal.send_replace(1);
        if let Ok(mut signals) = SIGNALS.lock() {
            signals.remove(&worker_record.operation_id);
        }
        match execution {
            Ok(result) => result,
            Err(error) => Err(anyhow!(error).context("Builder execution task interrupted")),
        }
    });
    Ok(worker)
}

/// Bound confirmation, not remote execution. Runtime implementations own a
/// cancellation-safe task and lease; dropping their future would detach that
/// task. Keep observing it after recording the deadline, retaining our local
/// lease until its actual outcome is available.
pub(crate) async fn observe_creation_budget<T>(
    deadline: impl Future<Output = ()>,
    creation: impl Future<Output = Result<T>>,
    expired: impl Future<Output = Result<()>>,
) -> Result<Option<Result<T>>> {
    tokio::pin!(creation);
    let started = std::sync::atomic::AtomicBool::new(false);
    let observed = std::future::poll_fn(|cx| {
        started.store(true, std::sync::atomic::Ordering::Relaxed);
        Future::poll(creation.as_mut(), cx)
    });
    tokio::pin!(observed);
    tokio::select! {
        biased;
        _ = deadline => {
            let recorded = expired.await;
            // Never start a fresh write after expiry. Once polled, however,
            // even a failed status write must not detach the in-flight task.
            if started.load(std::sync::atomic::Ordering::Relaxed) {
                let outcome = observed.await;
                tracing::warn!(succeeded = outcome.is_ok(), "Observed builder result after confirmation deadline; reconciliation is required");
            }
            recorded?;
            Ok(None)
        }
        result = &mut observed => Ok(Some(result)),
    }
}

pub(crate) async fn record_late_creation_result(
    state: &AppState,
    claimed: &UserAppOperationRecord,
    result: &Result<ContainerBasicInfo>,
) -> Result<()> {
    match result {
        Ok(info) => record_late_creation(state, claimed, info).await,
        Err(error) => {
            if matches!(
                error.downcast_ref::<container_runtime_api::ContainerRuntimeError>(),
                Some(container_runtime_api::ContainerRuntimeError::CreationCancelled)
            ) {
                let snapshot = state
                    .userapp_store
                    .get_operation(&claimed.app_id, &claimed.operation_id)
                    .await?
                    .ok_or_else(|| anyhow!("Cancelled builder operation disappeared"))?;
                if snapshot.lifecycle_id != claimed.lifecycle_id
                    || snapshot.executor_id != claimed.executor_id
                    || snapshot.request_fingerprint != claimed.request_fingerprint
                {
                    return Err(anyhow!("Builder cancellation belongs to a stale executor"));
                }
                state
                    .userapp_store
                    .finalize_builder_creation_cancellation(&snapshot)
                    .await?;
                return Ok(());
            }
            let Some(container_runtime_api::ContainerRuntimeError::RequestRejected(rejection)) =
                error.downcast_ref::<container_runtime_api::ContainerRuntimeError>()
            else {
                // A timeout/transport/cleanup failure remains uncertain.
                tracing::warn!(operation_id = %claimed.operation_id, error = %error,
                    "Late builder failure still requires runtime reconciliation");
                return Ok(());
            };
            let snapshot = state
                .userapp_store
                .get_operation(&claimed.app_id, &claimed.operation_id)
                .await?
                .ok_or_else(|| anyhow!("Late rejected builder operation disappeared"))?;
            if snapshot.lifecycle_id != claimed.lifecycle_id
                || snapshot.executor_id != claimed.executor_id
                || snapshot.request_fingerprint != claimed.request_fingerprint
            {
                return Err(anyhow!(
                    "Late builder rejection belongs to a stale executor"
                ));
            }
            state
                .userapp_store
                .finalize_builder_creation_rejection(&snapshot, rejection)
                .await?;
            Ok(())
        }
    }
}

/// Persist a late create response after physical identity verification, before
/// waiting for management readiness. The scanner completes that observation.
pub(crate) async fn record_late_creation(
    state: &AppState,
    claimed: &UserAppOperationRecord,
    info: &ContainerBasicInfo,
) -> Result<()> {
    let snapshot = state
        .userapp_store
        .get_operation(&claimed.app_id, &claimed.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Late builder operation disappeared"))?;
    if snapshot.state != UserAppOperationState::RecoveryRequired
        || snapshot.step != "creation_confirmation_timed_out"
        || snapshot.executor_id != claimed.executor_id
        || snapshot.lifecycle_id != claimed.lifecycle_id
        || snapshot.request_fingerprint != claimed.request_fingerprint
    {
        return Ok(());
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: claimed.app_id.clone(),
        lifecycle_id: claimed.lifecycle_id.clone(),
        operation_id: claimed.operation_id.clone(),
        executor_id: claimed
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Late builder executor missing"))?,
        request_fingerprint: claimed.request_fingerprint.clone(),
    };
    let target = crate::userapp_builder::adoption::capture_bound_target(state, &context).await?;
    let evidence = registration::attach_predecessor(
        state,
        &snapshot,
        shared_types::BuilderCreationEvidence {
            registration_predecessor: None,
            creation_lease_released: true,
            target,
            container: info.clone(),
        },
    )
    .await?;
    evidence
        .validate_operation(&snapshot)
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .confirm_builder_creation_recovery(&snapshot, &evidence, false)
        .await?;
    Ok(())
}

/// Bridge runtime completion to the database after the worker or connection
/// disappeared. The runtime releases only an acknowledged original lease.
pub(crate) async fn reconcile_runtime_receipt(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if !shared_types::userapp_builder_creation_needs_runtime_receipt(snapshot) {
        return Ok(false);
    }
    let Some(_local) = crate::userapp_builder::lifecycle::try_acquire(&snapshot.app_id).await
    else {
        return Ok(false);
    };
    if state
        .userapp_store
        .get_operation(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .as_ref()
        != Some(snapshot)
    {
        return Ok(false);
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        executor_id: snapshot
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Builder recovery executor missing"))?,
        request_fingerprint: snapshot.request_fingerprint.clone(),
    };
    let recovered = tokio::time::timeout(
        Duration::from_secs(30),
        state.runtime().recover_builder_creation(&context),
    )
    .await
    .context("Builder runtime receipt recovery deadline exceeded")?;
    let evidence = match recovered {
        Ok(Some(evidence)) => evidence,
        Ok(None) => return Ok(false),
        Err(container_runtime_api::ContainerRuntimeError::CreationCancelled) => {
            state
                .userapp_store
                .finalize_builder_creation_cancellation(snapshot)
                .await?;
            return Ok(true);
        }
        Err(error) => return Err(error.into()),
    };
    let evidence = registration::attach_predecessor(state, snapshot, evidence).await?;
    evidence
        .validate_operation(snapshot)
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .confirm_builder_creation_recovery(snapshot, &evidence, false)
        .await?;
    Ok(true)
}

/// A persisted create response is not a readiness result. Observe once per
/// recovery scan; never reissue creation or publish a project registration.
pub(crate) async fn reconcile_created(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if snapshot.kind != UserAppOperationKind::EnsureBuilder
        || snapshot.state != UserAppOperationState::RecoveryRequired
        || snapshot.step != "builder_created_observed"
    {
        return Ok(false);
    }
    let Some(_local) = crate::userapp_builder::lifecycle::try_acquire(&snapshot.app_id).await
    else {
        return Ok(false);
    };
    let mut evidence: shared_types::BuilderCreationEvidence =
        serde_json::from_value(snapshot.checkpoint.clone())?;
    evidence
        .validate_operation(snapshot)
        .map_err(anyhow::Error::msg)?;
    let Some(info) = tokio::time::timeout(
        Duration::from_secs(30),
        observe_created_ready(state, &evidence),
    )
    .await
    .context("Builder recovery observation deadline exceeded")??
    else {
        return Ok(false);
    };
    evidence.container = info;
    state
        .userapp_store
        .confirm_builder_creation_recovery(snapshot, &evidence, true)
        .await?;
    Ok(true)
}

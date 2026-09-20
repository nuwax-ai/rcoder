//! One transaction algorithm for PostgreSQL and Turso. Runtime effects never
//! execute here; the storage owner drains complete admitted transactions.
mod activity;
#[cfg(test)]
mod activity_tests;
#[cfg(test)]
mod admission_cancellation_tests;
mod codec;
#[cfg(test)]
mod concurrency_tests;
mod configuration;
#[cfg(all(test, feature = "userapp-turso"))]
mod configuration_tests;
#[cfg(feature = "userapp-turso")]
mod local_format;
#[cfg(all(test, feature = "userapp-turso"))]
mod local_tests;
mod ops;
#[cfg(all(test, feature = "pg"))]
mod pg_commit_reply_tests;
mod repo;
#[cfg(test)]
mod transaction_fault_tests;

use super::storage;
use crate::db::{owner::DatabaseOwner, schema::Backend};
use futures::future::BoxFuture;
use shared_types::*;
use toasty_core::driver::{IsolationLevel, operation::TransactionMode};

pub struct ToastyUserAppStore {
    owner: DatabaseOwner,
    backend: Backend,
    #[cfg(test)]
    admission_gate: Option<std::sync::Arc<admission_cancellation_tests::AdmissionGate>>,
}
impl ToastyUserAppStore {
    pub(crate) fn from_owner(owner: DatabaseOwner, backend: Backend) -> Self {
        Self {
            owner,
            backend,
            #[cfg(test)]
            admission_gate: None,
        }
    }

    async fn run<T, F>(&self, read_only: bool, body: F) -> Result<T, UserAppStoreError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a mut dyn toasty::Executor,
                Backend,
            ) -> BoxFuture<'a, Result<T, UserAppStoreError>>
            + Send
            + Clone
            + 'static,
    {
        let backend = self.backend;
        self.owner.execute(move |mut db| async move {
            // Replay the complete database-only transaction from the original
            // request. Never retry a commit/connection error: its result can be
            // unknown. Domain/CAS conflicts are known rolled-back attempts.
            for attempt in 0..3 {
                let mut builder = db.transaction_builder();
                if backend == Backend::Turso {
                    builder = builder.mode(TransactionMode::Immediate);
                } else if read_only {
                    builder = builder.isolation(IsolationLevel::RepeatableRead).read_only(true);
                }
                let mut tx = builder.begin().await?;
                match body.clone()(&mut tx, backend).await {
                    Ok(value) => { tx.commit().await?; return Ok(Ok(value)); }
                    Err(error) => {
                        if let Err(rollback) = tx.rollback().await {
                            anyhow::bail!("UserApp transaction failed ({error}); rollback failed ({rollback}); outcome requires verification");
                        }
                        if !read_only && matches!(error, UserAppStoreError::VersionConflict) && attempt < 2 {
                            tokio::time::sleep(std::time::Duration::from_millis(5 * (attempt + 1))).await;
                            continue;
                        }
                        return Ok(Err(error));
                    }
                }
            }
            anyhow::bail!("UserApp transaction retry budget exhausted")
        }).await.map_err(storage)?
    }
}
#[async_trait::async_trait]
impl super::control::UserAppStoreControl for ToastyUserAppStore {
    async fn shutdown(&self) -> Result<(), UserAppStoreError> {
        self.owner.shutdown().await.map_err(storage)
    }
}

#[async_trait::async_trait]
impl UserAppLifecycleStore for ToastyUserAppStore {
    async fn get_resource_binding(
        &self,
        service_type: &ServiceType,
        physical_uid: &str,
    ) -> Result<Option<UserAppResourceBinding>, UserAppStoreError> {
        let service_type = *service_type;
        let physical_uid = physical_uid.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::get_resource_binding(tx, backend, &service_type, &physical_uid).await
            })
        })
        .await
    }
    async fn commit_resource_binding(
        &self,
        binding: &UserAppResourceBinding,
        progress: &UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let binding = binding.clone();
        let progress = progress.clone();
        self.run(false, move |tx, backend| {
            Box::pin(
                async move { ops::commit_resource_binding(tx, backend, &binding, &progress).await },
            )
        })
        .await
    }
    async fn list_control_snapshots(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppControlSnapshot>, UserAppStoreError> {
        let after_app_id = after_app_id.map(str::to_owned);
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::list_control_snapshots(tx, backend, after_app_id.as_deref(), limit).await
            })
        })
        .await
    }
    async fn ensure_identity(
        &self,
        app_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError> {
        let app_id = app_id.to_owned();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::ensure_identity(tx, backend, &app_id).await })
        })
        .await
    }
    async fn get_application(
        &self,
        app_id: &str,
    ) -> Result<Option<UserAppLifecycleRecord>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(async move { ops::get_application(tx, backend, &app_id).await })
        })
        .await
    }
    async fn list_applications(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppLifecycleRecord>, UserAppStoreError> {
        let after_app_id = after_app_id.map(str::to_owned);
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::list_applications(tx, backend, after_app_id.as_deref(), limit).await
            })
        })
        .await
    }
    async fn patch_metadata(
        &self,
        patch: &UserAppMetadataPatch,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError> {
        let patch = patch.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::patch_metadata(tx, backend, &patch).await })
        })
        .await
    }
    async fn admit(
        &self,
        request: &UserAppAdmission,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError> {
        let request = request.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::admit(tx, backend, &request).await })
        })
        .await
    }
    async fn admit_with_input(
        &self,
        request: &UserAppAdmission,
        input: Option<&UserAppExecutionInput>,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError> {
        let request = request.clone();
        let input = input.cloned();
        #[cfg(test)]
        let admission_gate = self.admission_gate.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                let outcome = ops::admit_with_input(tx, backend, &request, input.as_ref()).await?;
                #[cfg(test)]
                if let Some(gate) = admission_gate {
                    gate.pause_before_commit().await;
                }
                Ok(outcome)
            })
        })
        .await
    }
    async fn admit_with_configuration(
        &self,
        request: &UserAppAdmission,
        input: &UserAppExecutionInput,
        pg: Option<&StartPgCredential>,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError> {
        let request = request.clone();
        let input = input.clone();
        let pg = pg.cloned();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::admit_with_configuration(tx, backend, &request, Some(&input), pg.as_ref())
                    .await
            })
        })
        .await
    }
    async fn read_execution_input(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<UserAppExecutionInput, UserAppStoreError> {
        let context = context.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::read_execution_input(tx, backend, &context).await })
        })
        .await
    }
    async fn bind_operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
        lifecycle_id: &str,
        deadline_epoch_ms: i64,
    ) -> Result<i64, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let operation_id = operation_id.to_owned();
        let lifecycle_id = lifecycle_id.to_owned();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::bind_operation_deadline(
                    tx,
                    backend,
                    &app_id,
                    &operation_id,
                    &lifecycle_id,
                    deadline_epoch_ms,
                )
                .await
            })
        })
        .await
    }
    async fn operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<i64>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let operation_id = operation_id.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(
                async move { ops::operation_deadline(tx, backend, &app_id, &operation_id).await },
            )
        })
        .await
    }
    async fn bind_operation_lease(
        &self,
        context: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> Result<(), UserAppStoreError> {
        let context = context.clone();
        let receipt = receipt.clone();
        self.run(false, move |tx, backend| {
            Box::pin(
                async move { ops::bind_operation_lease(tx, backend, &context, &receipt).await },
            )
        })
        .await
    }
    async fn get_operation_lease(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<UserAppOperationLeaseBinding>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let operation_id = operation_id.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(
                async move { ops::get_operation_lease(tx, backend, &app_id, &operation_id).await },
            )
        })
        .await
    }
    async fn terminal_operation_leases(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppOperationLeaseBinding>, UserAppStoreError> {
        let after = after.map(str::to_owned);
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::terminal_operation_leases(tx, backend, after.as_deref(), limit).await
            })
        })
        .await
    }
    async fn forget_operation_lease(
        &self,
        binding: &UserAppOperationLeaseBinding,
    ) -> Result<(), UserAppStoreError> {
        let binding = binding.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::forget_operation_lease(tx, backend, &binding).await })
        })
        .await
    }
    async fn reserve_completed_operation(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let snapshot = snapshot.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::reserve_completed_operation(tx, backend, &snapshot).await })
        })
        .await
    }
    async fn finalize_password_recovery(
        &self,
        snapshot: &UserAppOperationRecord,
        evidence: &DatabasePasswordEvidence,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let snapshot = snapshot.clone();
        let evidence = evidence.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::finalize_password_recovery(tx, backend, &snapshot, &evidence).await
            })
        })
        .await
    }
    async fn finalize_deploy_pg_recovery(
        &self,
        snapshot: &UserAppOperationRecord,
        evidence: &ExplicitDeploymentPasswordEvidence,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let snapshot = snapshot.clone();
        let evidence = evidence.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::finalize_deploy_pg_recovery(tx, backend, &snapshot, &evidence).await
            })
        })
        .await
    }
    async fn confirm_database_preparation_recovery(
        &self,
        snapshot: &UserAppOperationRecord,
        evidence: &DatabasePreparationEvidence,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let snapshot = snapshot.clone();
        let evidence = evidence.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::confirm_database_preparation_recovery(tx, backend, &snapshot, &evidence).await
            })
        })
        .await
    }
    async fn advance(
        &self,
        progress: &UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let progress = progress.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { ops::advance(tx, backend, &progress).await })
        })
        .await
    }
    async fn get_operation_by_request(
        &self,
        app_id: &str,
        request_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let request_id = request_id.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::get_operation_by_request(tx, backend, &app_id, &request_id).await
            })
        })
        .await
    }
    async fn get_operation(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let operation_id = operation_id.to_owned();
        self.run(true, move |tx, backend| {
            Box::pin(async move { ops::get_operation(tx, backend, &app_id, &operation_id).await })
        })
        .await
    }
    async fn unfinished_operations(
        &self,
        after_operation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppOperationRecord>, UserAppStoreError> {
        let after_operation_id = after_operation_id.map(str::to_owned);
        self.run(true, move |tx, backend| {
            Box::pin(async move {
                ops::unfinished_operations(tx, backend, after_operation_id.as_deref(), limit).await
            })
        })
        .await
    }
    async fn recreate(
        &self,
        app_id: &str,
        expected_lifecycle_id: &str,
        request_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let expected_lifecycle_id = expected_lifecycle_id.to_owned();
        let request_id = request_id.to_owned();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                ops::recreate(tx, backend, &app_id, &expected_lifecycle_id, &request_id).await
            })
        })
        .await
    }
}

impl ToastyUserAppStore {
    pub async fn shutdown(&self) -> Result<(), UserAppStoreError> {
        self.owner.shutdown().await.map_err(storage)
    }

    #[cfg(feature = "userapp-turso")]
    pub async fn open_exclusive(path: &std::path::Path) -> Result<Self, UserAppStoreError> {
        use crate::db::{
            driver::{ConnectionPolicy, PolicyDriver},
            models,
            schema::{self, Component},
        };
        let requested = path.to_path_buf();
        let (path, lock) =
            tokio::task::spawn_blocking(move || super::exclusive_directory::acquire(&requested))
                .await
                .map_err(storage)??;
        if !path.exists()
            && path
                .parent()
                .is_some_and(|dir| dir.join("userapp.sqlite3").exists())
        {
            return Err(UserAppStoreError::InvalidOperation("data directory contains an old development database; use a fresh directory for the Toasty baseline".into()));
        }
        local_format::prepare(&path).map_err(storage)?;
        let owner = DatabaseOwner::open(256, 1, lock, move || async move {
            let driver = PolicyDriver::new(
                toasty_driver_turso::Turso::file(&path),
                ConnectionPolicy::Turso,
            );
            let mut db = toasty::Db::builder()
                .models(models::storage_models())
                .max_pool_size(1)
                .build(driver)
                .await?;
            schema::initialize(&mut db, Backend::Turso, &[Component::Userapp]).await?;
            local_format::mark_ready(&path)?;
            Ok(db)
        })
        .await
        .map_err(storage)?;
        let store = Self::from_owner(owner, Backend::Turso);
        let result = store
            .run(false, |tx, backend| {
                Box::pin(async move { ops::quarantine_local_restart(tx, backend).await })
            })
            .await;
        if let Err(error) = result {
            if let Err(close) = store.shutdown().await {
                return Err(storage(anyhow::anyhow!(
                    "local recovery initialization failed ({error}); shutdown failed ({close})"
                )));
            }
            return Err(error);
        }
        Ok(store)
    }
}

#[cfg(feature = "pg")]
impl ToastyUserAppStore {
    pub async fn connect(
        config: &crate::config::PostgresConfig,
    ) -> Result<Self, UserAppStoreError> {
        let owner = crate::db::postgres::open(config, vec![crate::db::schema::Component::Userapp])
            .await
            .map_err(storage)?;
        Ok(Self::from_owner(owner, Backend::Postgres))
    }
}

#[async_trait::async_trait]
impl UserAppRuntimeConfigurationStore for ToastyUserAppStore {
    async fn save_runtime_configuration(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
        request: &SaveRuntimeConfigurationRequest,
    ) -> Result<SavedRuntimeConfiguration, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let request = request.clone();
        self.run(false, move |tx, backend| {
            Box::pin(
                async move { configuration::save(tx, backend, &app_id, scope, &request).await },
            )
        })
        .await
    }
    async fn runtime_configuration_status(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        scope: UserAppOperationScope,
    ) -> Result<Option<RuntimeConfigurationStatus>, UserAppStoreError> {
        let app_id = app_id.to_owned();
        let lifecycle_id = lifecycle_id.to_owned();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                configuration::read_status(tx, backend, &app_id, &lifecycle_id, scope).await
            })
        })
        .await
    }
    async fn operation_runtime_configuration(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<Option<RuntimeConfigurationCapture>, UserAppStoreError> {
        let context = context.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move { configuration::read(tx, backend, &context).await })
        })
        .await
    }
    async fn bind_runtime_configuration_target(
        &self,
        context: &UserAppExecutionContext,
        config_version: i64,
        target: &RuntimeConfigurationTarget,
    ) -> Result<(), UserAppStoreError> {
        let context = context.clone();
        let target = target.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                configuration::bind(tx, backend, &context, config_version, &target).await
            })
        })
        .await
    }
    async fn record_runtime_configuration_result(
        &self,
        context: &UserAppExecutionContext,
        config_version: i64,
        target: &RuntimeConfigurationTarget,
        credentials: CredentialApplicationState,
        business: BusinessStartupState,
    ) -> Result<(), UserAppStoreError> {
        let context = context.clone();
        let target = target.clone();
        self.run(false, move |tx, backend| {
            Box::pin(async move {
                configuration::record(
                    tx,
                    backend,
                    &context,
                    config_version,
                    &target,
                    credentials,
                    business,
                )
                .await
            })
        })
        .await
    }
}

/// Offline E2E observation never initializes schemas or quarantines live records.
/// The directory lease is held through owner shutdown, preventing concurrent RCoder access.
#[cfg(feature = "userapp-turso")]
pub async fn offline_snapshot(
    path: &std::path::Path,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    use crate::db::{
        driver::{ConnectionPolicy, PolicyDriver},
        models,
        schema::{self, Component},
    };
    let requested = path.to_path_buf();
    let (path, lock) =
        tokio::task::spawn_blocking(move || super::exclusive_directory::acquire(&requested))
            .await??;
    local_format::require_ready(&path)?;
    let owner = DatabaseOwner::open(1, 1, lock, move || async move {
        let mut db = toasty::Db::builder()
            .models(models::storage_models())
            .max_pool_size(1)
            .build(PolicyDriver::new(
                toasty_driver_turso::Turso::file(&path),
                ConnectionPolicy::Turso,
            ))
            .await?;
        schema::verify_existing(&mut db, Backend::Turso, Component::Userapp).await?;
        Ok(db)
    })
    .await?;
    let result = owner
        .execute(|mut db| async move {
            let mut tx = db.transaction().await?;
            let mut applications = Vec::new();
            for row in models::Application::all().exec(&mut tx).await? {
                let application = repo::app(&mut tx, &row.app_id).await?.ok_or_else(|| {
                    anyhow::anyhow!("Application disappeared in offline snapshot")
                })?;
                applications.push(serde_json::to_string(&application)?);
            }
            let mut operations = Vec::new();
            for row in models::Operation::all().exec(&mut tx).await? {
                operations.push(serde_json::to_string(&codec::operation(row)?)?);
            }
            tx.commit().await?;
            applications.sort();
            operations.sort();
            Ok((applications, operations))
        })
        .await;
    owner.shutdown().await?;
    result
}

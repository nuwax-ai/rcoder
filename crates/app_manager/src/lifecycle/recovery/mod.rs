//! Replay only unclaimed controls with their original durable inputs. A retained
//! runtime marker/lease blocks this entry; elapsed time never grants ownership.
//!
//! 结构：本文件是存储身份恢复与恢复装配；热部署恢复协调在 [`hot`]，
//! 控制操作恢复/重试/续跑在 [`control`]。

mod control;
mod hot;

use crate::models::{AppOperationError, AppResult};
use crate::service::AppService;
use crate::utils::map_runtime_error;

impl AppService {
    /// Capture the prod mutation target, falling back to the durable physical
    /// binding when the resource's native identity predates the current
    /// lifecycle (adopted Docker containers keep immutable labels). The
    /// expected-version precondition is re-applied on the bound path.
    pub(crate) async fn capture_bound_app_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> AppResult<shared_types::UserAppMutationTarget> {
        match self
            .runtime
            .capture_app_mutation_target(context, expected_version)
            .await
        {
            Ok(target) => Ok(target),
            Err(error) => {
                let container_runtime_api::ContainerRuntimeError::Conflict(_) = &error else {
                    return Err(map_runtime_error("Capture application target", error));
                };
                // Only the identity rejection is binding-recoverable.
                let uid = match self
                    .runtime
                    .adopted_app_physical_uid(context)
                    .await
                    .map_err(|error| map_runtime_error("Probe application physical UID", error))?
                {
                    Some(uid) => uid,
                    None => {
                        return Err(map_runtime_error("Capture application target", error));
                    }
                };
                let binding = self
                    .metadata
                    .store
                    .get_resource_binding(&shared_types::ServiceType::Userapp, &uid)
                    .await?;
                let Some(binding) =
                    binding.filter(|binding| binding.validate(context, &uid).is_ok())
                else {
                    return Err(map_runtime_error("Capture application target", error));
                };
                let target = self
                    .runtime
                    .capture_bound_app_control(context, &binding)
                    .await
                    .map_err(|error| {
                        map_runtime_error("Capture bound application target", error)
                    })?;
                if let Some(expected) = expected_version
                    && target.resource.resource_version.as_deref() != Some(expected)
                {
                    return Err(AppOperationError::Conflict(
                        "Application mutation target version changed".into(),
                    ));
                }
                Ok(target)
            }
        }
    }

    pub async fn verify_recovered_storage(
        &self,
        app_id: &str,
        scope: shared_types::UserAppOperationScope,
    ) -> AppResult<()> {
        let Some(app) = self.metadata.store.get_application(app_id).await? else {
            return Ok(());
        };
        if app.state != shared_types::UserAppLifecycleState::Active {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        let Some(witness) = self
            .metadata
            .store
            .get_recovery_witness(app_id, &app.lifecycle_id)
            .await?
        else {
            return Ok(());
        };
        let volumes = match scope {
            shared_types::UserAppOperationScope::Dev => &witness.dev_volumes,
            shared_types::UserAppOperationScope::Prod => &witness.prod_volumes,
            shared_types::UserAppOperationScope::Application => {
                return Err(AppOperationError::Validation(
                    "Storage verification requires an explicit scope".into(),
                ));
            }
        };
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id,
            operation_id: "recovery-storage-observation".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        self.runtime
            .verify_recovered_volumes(&context, scope, volumes)
            .await
            .map_err(|error| map_runtime_error("Verify recovered storage", error))
    }

    /// Reconstruct a missing control root from consistent managed resource
    /// identities. No runtime writes, deployment, or historical operation replay.
    pub async fn discover_missing_identity(
        &self,
        app_id: &str,
    ) -> AppResult<Option<shared_types::UserAppLifecycleRecord>> {
        crate::utils::validate_app_id(app_id)?;
        let existing = crate::service::restart_wait::prepare(async {
            Ok(self.metadata.store.get_application(app_id).await?)
        })
        .await?;
        if let Some(existing) = &existing {
            if existing.state != shared_types::UserAppLifecycleState::Active {
                return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
            }
            if existing.lifecycle_epoch != 1
                || existing.metadata_revision != 1
                || existing.active_operations != shared_types::UserAppActiveOperations::default()
            {
                return Ok(Some(existing.clone()));
            }
        }
        let found = crate::service::restart_wait::prepare(async {
            self.runtime
                .discover_application_identity(app_id)
                .await
                .map_err(|e| map_runtime_error("Discover existing lifecycle", e))
        })
        .await?;
        let Some(found) = found else {
            return Ok(existing);
        };
        if existing
            .as_ref()
            .is_some_and(|app| app.lifecycle_id == found.lifecycle_id)
        {
            return Ok(existing);
        }
        found.validate().map_err(AppOperationError::Validation)?;
        let confirmed = crate::service::restart_wait::prepare(async {
            self.runtime
                .discover_application_identity(app_id)
                .await
                .map_err(|e| map_runtime_error("Confirm existing lifecycle", e))
        })
        .await?;
        if confirmed.as_ref() != Some(&found) {
            return Err(AppOperationError::Conflict(
                "Managed resource inventory changed during registration recovery".into(),
            ));
        }
        // Registration is a control-root write. Claim ownership only after
        // bounded discovery, then resolve the write even if the receiver goes
        // away. A confirmed registration is not business admission: return to
        // Waiting and honor a disconnect before taking an operation lease.
        crate::service::restart_wait::begin_admission()?;
        let restored = self
            .metadata
            .store
            .restore_discovered_identity(&found)
            .await
            .map_err(AppOperationError::from);
        if match &restored {
            Ok(_) => true,
            Err(error) => !error.requires_recovery(),
        } {
            crate::service::restart_wait::rejected_before_admission();
        }
        restored.map(Some)
    }

    pub fn set_builder_recovery(
        &self,
        recovery: std::sync::Arc<dyn shared_types::UserAppBuilderRecovery>,
    ) -> AppResult<()> {
        *self.builder_recovery.write().map_err(|_| {
            AppOperationError::Backend("Builder recovery registry lock poisoned".into())
        })? = Some(recovery);
        Ok(())
    }

    /// Resume the same unclaimed operation; a retry never clears a runtime lease
    /// or invents missing command inputs from current deployment configuration.
    /// 只读观察围栏应用的运行时实况（诊断用；观察结果不构成任何裁决依据）。
    async fn observe_uncertain_runtime(&self, app_id: &str) -> String {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(status)) => format!(
                "observed runtime deployment present (phase={}, ready={})",
                status.phase, status.ready_replicas
            ),
            Ok(None) => "observed runtime deployment absent at observation time (not proof of \
                 absence; a late write may still land)"
                .to_string(),
            Err(error) => format!("runtime observation unavailable: {error}"),
        }
    }
}

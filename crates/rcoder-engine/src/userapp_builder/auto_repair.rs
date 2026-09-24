//! Low-priority recovery of legacy Docker Published builders missing host ports.
//! Detection is read-only; only the durable idle-only compute admission may
//! authorize replacement. One deterministic request identity is used per UID.
use crate::app_state::AppState;
use anyhow::Result;
#[cfg(feature = "deploy-host")]
use anyhow::anyhow;
#[cfg(feature = "deploy-host")]
use sha2::{Digest as _, Sha256};
use shared_types::ComputeControlState;
#[cfg(feature = "deploy-host")]
use shared_types::{
    ComputeControlAction, ComputeControlRequest, ServiceType, UserAppExecutionContext,
    UserAppLifecycleState, UserAppOperationScope, UserAppStoreError,
};
#[cfg(feature = "deploy-host")]
use std::sync::Arc;

pub(super) struct RepairNotice {
    pub operation_id: String,
    pub state: ComputeControlState,
}

pub(super) async fn repair_if_needed(
    state: &AppState,
    app_id: &str,
    expected_uid: Option<&str>,
) -> Result<Option<RepairNotice>> {
    #[cfg(feature = "deploy-host")]
    if !shared_types::is_deploy_host() || shared_types::deploy_host_reach::is_direct() {
        return Ok(None);
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        let _ = (state, app_id, expected_uid);
        Ok(None)
    }
    #[cfg(feature = "deploy-host")]
    {
        let Some(app) = state.userapp_store.get_application(app_id).await? else {
            return Ok(None);
        };
        if app.state != UserAppLifecycleState::Active {
            return Ok(None);
        }
        let context = UserAppExecutionContext {
            app_id: app_id.to_owned(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: "read-only-repair-check".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        let target = super::adoption::capture_bound_target(state, &context).await?;
        let Some(resource) = target.workload.as_ref() else {
            return Ok(None);
        };
        if expected_uid.is_some_and(|uid| uid != resource.uid) {
            return Ok(None);
        }
        let Some(missing) = state
            .runtime()
            .missing_builder_published_ports(&target)
            .await?
        else {
            return Ok(None);
        };
        if missing.is_empty() {
            return Ok(None);
        }
        let uid_digest = hex::encode(Sha256::digest(resource.uid.as_bytes()));
        let request_id = format!(
            "{}{}",
            shared_types::AUTOMATIC_REPAIR_REQUEST_PREFIX,
            &uid_digest[..52]
        );
        let flight = state.userapp_op_flight.guard()?;
        let record = match state
            .userapp_store
            .admit_idle_compute_repair(&ComputeControlRequest {
                app_id: app_id.to_owned(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Dev,
                action: ComputeControlAction::Restart,
                restart_image_roll: false,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id,
                request_fingerprint: super::compute_control::auto_repair_fingerprint(
                    app_id,
                    &app.lifecycle_id,
                    &resource.uid,
                ),
            })
            .await
        {
            Ok(record) => record,
            Err(UserAppStoreError::VersionConflict) => {
                return Err(anyhow!(
                    "Published builder {app_id} is missing ports {missing:?}; automatic repair deferred because another operation owns the scope"
                ));
            }
            Err(error) => return Err(error.into()),
        };
        tracing::warn!(app_id, uid = %resource.uid, missing_ports = ?missing,
            operation_id = %record.operation_id, "Published builder repair admitted");
        let operation_id = record.operation_id.clone();
        let repair_state = record.state;
        if record.state == ComputeControlState::Pending {
            let state = Arc::new(state.clone());
            tokio::spawn(async move {
                let _flight = flight;
                if let Err(error) = super::compute_control::execute_pending(&state, record).await {
                    tracing::error!(%error, "Automatic Published builder repair requires inspection");
                }
            });
        }
        Ok(Some(RepairNotice {
            operation_id,
            state: repair_state,
        }))
    }
}

#[cfg(feature = "deploy-host")]
pub(crate) fn start_scan(state: std::sync::Weak<AppState>) {
    tokio::spawn(async move {
        let Some(state) = state.upgrade() else {
            return;
        };
        if !shared_types::is_deploy_host() || shared_types::deploy_host_reach::is_direct() {
            return;
        }
        let docker = match bollard::Docker::connect_with_local_defaults() {
            Ok(docker) => docker,
            Err(error) => {
                tracing::warn!(%error, "Published builder scan cannot connect to Docker");
                return;
            }
        };
        let containers = match docker
            .list_containers(None::<bollard::query_parameters::ListContainersOptions>)
            .await
        {
            Ok(containers) => containers,
            Err(error) => {
                tracing::warn!(%error, "Published builder scan cannot list Docker containers");
                return;
            }
        };
        let candidates = containers
            .into_iter()
            .filter_map(|container| {
                let labels = container.labels?;
                if labels.get("service-type")?.as_str()
                    != ServiceType::UserappBuilder.container_family_key()
                {
                    return None;
                }
                Some((labels.get("identifier")?.clone(), container.id?))
            })
            .take(32);
        for (app_id, uid) in candidates {
            if let Err(error) = repair_if_needed(&state, &app_id, Some(&uid)).await {
                tracing::warn!(app_id, uid, %error, "Published builder scan deferred repair");
            }
        }
    });
}

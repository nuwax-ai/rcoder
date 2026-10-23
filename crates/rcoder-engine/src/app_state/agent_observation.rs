//! Metadata convergence after a positive absence observation. No physical
//! resources are deleted and no replacement execution is created here.
use super::AppState;
use anyhow::{Context, Result};
use shared_types::{ProjectAndContainerInfo, ServiceType, persistence::PersistenceWriteOutcome};
use std::time::Duration;

impl AppState {
    pub(crate) async fn retire_absent_ordinary_registration(
        &self,
        expected: &ProjectAndContainerInfo,
        idle_timeout: Duration,
    ) -> Result<bool> {
        let budget =
            Duration::from_secs(self.config.cleanup_config.orphan_scan_budget_seconds.max(1));
        tokio::time::timeout(
            budget,
            self.retire_absent_ordinary_registration_inner(expected, idle_timeout),
        )
        .await
        .context("Agent absence observation or metadata retirement remains unconfirmed")?
    }

    async fn retire_absent_ordinary_registration_inner(
        &self,
        expected: &ProjectAndContainerInfo,
        idle_timeout: Duration,
    ) -> Result<bool> {
        let Some(family) = expected.service_type() else {
            return Ok(false);
        };
        match family {
            ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::WebAgentRunner => {}
            // Builder retirement remains governed by its lifecycle/control flow.
            ServiceType::UserappBuilder | ServiceType::Userapp => return Ok(false),
        }
        let Some(physical) = expected.container_info() else {
            return Ok(false);
        };
        let generation = expected.persistence_identity().generation.clone();
        let identifier = family
            .container_identifier(
                expected.pod_id(),
                expected.user_id(),
                Some(expected.project_id()),
            )?
            .to_owned();
        // Fast observation avoids an exclusive admission for every live RPC
        // failure; the authoritative observation is repeated while holding it.
        if !self
            .runtime()
            .agent_compute_absent(&identifier, &family, &physical)
            .await?
        {
            return Ok(false);
        }
        let guard = self
            .acquire_agent_workload(
                &identifier,
                &family,
                rcoder_storage::WorkloadAdmissionMode::Exclusive,
            )
            .await?;
        let Some(current) = self
            .projects
            .refresh_project_registration(expected.project_id())
            .await?
        else {
            return Ok(false);
        };
        if current.persistence_identity().generation != generation
            || current.container_info().as_ref().is_none_or(|value| {
                value.container_id != physical.container_id
                    || value.workload_uid != physical.workload_uid
                    || value.container_name != physical.container_name
            })
        {
            return Ok(false);
        }
        let frame = self
            .projects
            .capture_workload_registration(&guard, guard.key().logical_name())
            .await?;
        if !frame.project_ids().contains(&expected.project_id())
            || frame.physical_uid() != Some(physical.container_id.as_str())
        {
            return Ok(false);
        }
        for id in frame.project_ids() {
            let Some(reference) = self.projects.refresh_project_registration(id).await? else {
                return Ok(false);
            };
            if chrono::Utc::now()
                .signed_duration_since(reference.last_activity())
                .to_std()
                .is_ok_and(|idle| idle >= idle_timeout)
            {
                continue;
            }
            return Ok(false);
        }
        guard.verify_current().await?;
        if !self
            .runtime()
            .agent_compute_absent(&identifier, &family, &physical)
            .await?
        {
            return Ok(false);
        }
        let outcome = self
            .retire_stopped_agent_registration(&guard, &frame)
            .await?;
        match outcome {
            PersistenceWriteOutcome::Committed => {
                tracing::info!(project_id = expected.project_id(), %identifier,
                    container_id = %physical.container_id,
                    "Absent Agent registration retired; history witnesses and volumes retained");
                Ok(true)
            }
            PersistenceWriteOutcome::Superseded => Ok(false),
            PersistenceWriteOutcome::Deferred { reason } => {
                anyhow::bail!("Absent Agent registration retirement remains unconfirmed: {reason}")
            }
        }
    }
}

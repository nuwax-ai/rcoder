//! Serialized publication transactions. Failure is safe only after the previous
//! complete graph is confirmed again; unknown restoration retains write fencing.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use pingap_config::PingapConfig;
use tokio::time::Instant;

use super::{
    admin_probe::{self, AdminEndpoint},
    apply_status::{ApplyStatus, ConfirmedPublication},
    compiler::{self, CompileOutcome, PUBLICATION_OBJECT},
};

async fn observe(endpoint: &AdminEndpoint, deadline: Instant) -> Result<ApplyStatus> {
    tokio::time::timeout_at(deadline, admin_probe::fetch_apply_status(endpoint))
        .await
        .context("proxy publication observation deadline exhausted")?
}

fn same_instance(status: &ApplyStatus, expected: &ConfirmedPublication) -> Result<()> {
    ensure!(
        status.schema_version == 1
            && status.process_id == expected.process_id
            && status.process_instance_id == expected.instance_id,
        "proxy process identity changed; restoration is not authorized"
    );
    Ok(())
}

async fn baseline(
    target: &Path,
    endpoint: &AdminEndpoint,
    deadline: Instant,
) -> Result<(PingapConfig, ConfirmedPublication)> {
    let status = observe(endpoint, deadline).await?;
    status.validate_identity()?;
    let applied = status
        .applied
        .as_ref()
        .context("proxy has no completed application result")?;
    let snapshot = tokio::time::timeout_at(deadline, tokio::fs::read(target))
        .await
        .context("read applied graph deadline exhausted")?;
    let config = snapshot
        .ok()
        .and_then(|bytes| PingapConfig::new(&bytes, true).ok());
    let config = match config {
        Some(config) if compiler::configuration_digest(&config)? == applied.config_digest => config,
        _ => {
            // An incomplete active file cannot permanently prevent source retry.
            // Only this owner's previously confirmed immutable snapshot grants
            // recovery authority; no directory scan or arbitrary old UUID does.
            let (outcome, confirmation) = compiler::confirmed_publication().context(
                "active file differs from the applied graph and no trusted snapshot exists",
            )?;
            same_instance(&status, &confirmation)?;
            ensure!(
                status.confirmed(&outcome)?.is_some(),
                "stored proxy receipt is no longer applied"
            );
            let bytes = tokio::time::timeout_at(deadline, tokio::fs::read(&outcome.config_path))
                .await
                .context("read confirmed snapshot deadline exhausted")??;
            let config = PingapConfig::new(&bytes, true).context("decode confirmed snapshot")?;
            ensure!(
                compiler::configuration_digest(&config)? == applied.config_digest,
                "confirmed immutable snapshot was modified"
            );
            config
        }
    };
    let confirmation = ConfirmedPublication {
        process_id: status.process_id,
        instance_id: status.process_instance_id,
        publication_id: applied
            .operation_id
            .clone()
            .context("applied publication identity missing")?,
        config_hash: applied.config_hash.clone(),
        config_digest: applied.config_digest.clone(),
        attempt_id: applied.attempt_id,
    };
    Ok((config, confirmation))
}

pub async fn publish_standby_confirmed(
    root: &Path,
    endpoint: &AdminEndpoint,
) -> Result<CompileOutcome> {
    publish_standby_confirmed_to_path(root, &compiler::active_config_path(root), endpoint).await
}

pub async fn publish_standby_confirmed_to_path(
    root: &Path,
    target: &Path,
    endpoint: &AdminEndpoint,
) -> Result<CompileOutcome> {
    let deadline = Instant::now() + admin_probe::CONFIRM_BUDGET;
    publish_standby_confirmed_to_path_until(root, target, endpoint, deadline).await
}

pub async fn publish_standby_confirmed_to_path_until(
    root: &Path,
    target: &Path,
    endpoint: &AdminEndpoint,
    deadline: Instant,
) -> Result<CompileOutcome> {
    let _guard = tokio::time::timeout_at(deadline, compiler::publication_guard())
        .await
        .context("proxy publication gate deadline exhausted before mutation")?;
    tokio::time::timeout_at(deadline, compiler::wait_for_file_replacements())
        .await
        .context("previous file replacement remains pending")?;
    let (previous, instance) = baseline(target, endpoint, deadline).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let content = toml::to_string_pretty(&previous)?;
    let (content, _) = compiler::build_standby_from_topology(&id, Some(&content))?;
    let config = PingapConfig::new(content.as_bytes(), true)?;
    let candidate = compiler::persist_candidate(root, &config, &id, 503).await?;
    transact(
        root, target, &candidate, previous, &instance, endpoint, deadline,
    )
    .await?;
    Ok(candidate)
}

/// Used by management reload when no builtin actor owns the entry. Callers
/// serialize this with orchestration through the existing auxiliary writer.
pub async fn publish_confirmed(
    root: &Path,
    target: &Path,
    candidate: &CompileOutcome,
    endpoint: &AdminEndpoint,
) -> Result<ConfirmedPublication> {
    let deadline = Instant::now() + admin_probe::CONFIRM_BUDGET;
    let _guard = tokio::time::timeout_at(deadline, compiler::publication_guard())
        .await
        .context("proxy publication gate deadline exhausted before mutation")?;
    tokio::time::timeout_at(deadline, compiler::wait_for_file_replacements())
        .await
        .context("previous file replacement remains pending")?;
    let (previous, instance) = baseline(target, endpoint, deadline).await?;
    transact(
        root, target, candidate, previous, &instance, endpoint, deadline,
    )
    .await
}

async fn transact(
    root: &Path,
    target: &Path,
    candidate: &CompileOutcome,
    previous: PingapConfig,
    instance: &ConfirmedPublication,
    endpoint: &AdminEndpoint,
    deadline: Instant,
) -> Result<ConfirmedPublication> {
    let previous_probes = compiler::confirmed_publication()
        .filter(|(_, receipt)| {
            receipt.publication_id == instance.publication_id
                && receipt.config_digest == instance.config_digest
        })
        .map(|(outcome, _)| outcome.business_probes)
        .context("previous graph lacks its owned business verification contract")?;
    let bytes = tokio::time::timeout_at(deadline, tokio::fs::read(&candidate.config_path))
        .await
        .context("read publication candidate deadline exhausted")??;
    let config = PingapConfig::new(&bytes, true)?;
    ensure!(
        compiler::configuration_digest(&config)? == candidate.config_digest,
        "validated proxy candidate was modified"
    );
    ensure!(
        Instant::now() < deadline,
        "proxy publication budget exhausted before mutation"
    );
    // Do not detach a blocking rename by timing out its JoinHandle: it could
    // overwrite a later rollback. Await its known completion, then evaluate the
    // original absolute budget. A late completion remains unknown, never success.
    let forward = match compiler::replace_config_bytes_until(target, bytes, deadline).await {
        Ok(()) => {
            let budget = deadline
                .saturating_duration_since(Instant::now())
                .saturating_sub(std::time::Duration::from_secs(6));
            admin_probe::wait_for_publication(endpoint, candidate, budget).await
        }
        Err(error) => Err(error),
    };
    match forward {
        Ok(confirmation)
            if confirmation.process_id == instance.process_id
                && confirmation.instance_id == instance.instance_id =>
        {
            compiler::record_confirmed_publication(candidate, &confirmation)?;
            Ok(confirmation)
        }
        result => {
            let error = result
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("proxy instance changed during publication"));
            match rollback_publication(
                root,
                target,
                previous,
                endpoint,
                instance,
                &previous_probes,
                deadline,
            )
            .await
            {
                Ok((outcome, confirmation)) => {
                    compiler::record_confirmed_publication(&outcome, &confirmation)?;
                    Err(error)
                        .context("publication rejected; previous graph restored, business retained")
                }
                Err(restore_error) => Err(crate::supervisor::ShutdownUnconfirmed(format!(
                    "publication rejected: {error:#}; proxy restoration unknown: {restore_error:#}"
                ))
                .into()),
            }
        }
    }
}

pub async fn rollback_publication(
    root: &Path,
    target: &Path,
    mut previous: PingapConfig,
    endpoint: &AdminEndpoint,
    expected_instance: &ConfirmedPublication,
    previous_business_probes: &[compiler::ServiceProbeTarget],
    deadline: Instant,
) -> Result<(CompileOutcome, ConfirmedPublication)> {
    same_instance(&observe(endpoint, deadline).await?, expected_instance)?;
    let status = previous
        .plugins
        .get(PUBLICATION_OBJECT)
        .and_then(|plugin| plugin.get("status"))
        .and_then(toml::Value::as_integer)
        .context("previous graph lacks a trusted publication marker")?;
    ensure!(
        status == 200 || status == 503,
        "invalid previous publication probe status"
    );
    previous.plugins.remove(PUBLICATION_OBJECT);
    previous.locations.remove(PUBLICATION_OBJECT);
    for server in previous.servers.values_mut() {
        if let Some(locations) = &mut server.locations {
            locations.retain(|name| name != PUBLICATION_OBJECT);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let config = compiler::publication_config(previous, &id, status as u16)?;
    let mut outcome = compiler::persist_candidate(root, &config, &id, status as u16).await?;
    if status == 200 {
        outcome.business_probes = previous_business_probes.to_vec();
    }
    ensure!(
        Instant::now() < deadline,
        "restoration budget exhausted before mutation"
    );
    let bytes = tokio::fs::read(&outcome.config_path).await?;
    compiler::replace_config_bytes_until(target, bytes, deadline).await?;
    let confirmation = admin_probe::wait_for_publication(
        endpoint,
        &outcome,
        deadline.saturating_duration_since(Instant::now()),
    )
    .await?;
    ensure!(
        confirmation.process_id == expected_instance.process_id
            && confirmation.instance_id == expected_instance.instance_id,
        "proxy process changed during restoration"
    );
    Ok((outcome, confirmation))
}

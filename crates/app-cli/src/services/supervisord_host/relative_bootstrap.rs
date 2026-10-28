//! Retire an unstarted relative-path bootstrap without guessing its old cwd.
use super::*;

struct RelativeBootstrap {
    path: PathBuf,
    bytes: Vec<u8>,
    spec: ServiceSpecFile,
}

fn relative_bootstrap(release: &str) -> Result<Option<RelativeBootstrap>> {
    let path = current_specification_path(release)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read historical proxy bootstrap"),
    };
    let spec: ServiceSpecFile = toml::from_str(std::str::from_utf8(&bytes)?)
        .context("decode historical proxy bootstrap")?;
    anyhow::ensure!(
        spec.release_id == release && spec.service_id == "pingap",
        "historical proxy bootstrap identity does not match its record"
    );
    Ok(
        admin_probe::proxy_spec_relative_config_legacy(&spec).then_some(RelativeBootstrap {
            path,
            bytes,
            spec,
        }),
    )
}

impl SupervisordHost {
    /// This is an owner-authorized mutation, before credential recovery. A
    /// relative historical -c never authorizes a path or a running process.
    pub(super) async fn recover_relative_bootstrap_until(
        &self,
        args: &RuntimeArgs,
        cancel: &tokio_util::sync::CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let configured = configured_entry_spec(&self.conf_path)?;
        let mut records = Vec::new();
        if let Some(record) = relative_bootstrap(crate::svc_spec::RESIDENT_SPEC_ID)? {
            records.push(record);
        }
        if let Some(spec) = &configured
            && spec.release_id != crate::svc_spec::RESIDENT_SPEC_ID
            && let Some(record) = relative_bootstrap(&spec.release_id)?
        {
            records.push(record);
        }
        if records.is_empty() {
            return Ok(());
        }

        let _control = supervisor::resident::control_publication(deadline).await?;
        self.settle_entry_writes_until(deadline).await?;
        let scope = supervisor::resident::owner_scope()?
            .context("relative proxy bootstrap has no retained owner capability")?;
        self.authorize_current_entry_domain_until(&scope, deadline)
            .await?;
        let spec_root = std::fs::canonicalize(crate::svc_spec::spec_root())
            .context("resolve current proxy specification directory")?;
        for record in &records {
            anyhow::ensure!(
                std::fs::canonicalize(record.path.parent().context("proxy record has no parent")?)?
                    .starts_with(&spec_root),
                "relative proxy bootstrap is outside the current specification directory"
            );
        }

        // RUNNING=false alone is insufficient: a stopped-looking record may
        // still carry an executing PID or be awaiting a late supervisor write.
        self.require_inert_relative_bootstrap_until(deadline)
            .await?;
        anyhow::ensure!(
            serde_json::to_value(configured_entry_spec(&self.conf_path)?)?
                == serde_json::to_value(&configured)?,
            "proxy restart configuration changed during bootstrap recovery"
        );
        for record in &records {
            anyhow::ensure!(
                std::fs::read(&record.path)? == record.bytes,
                "historical proxy bootstrap changed during recovery"
            );
        }
        anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
        scope.record_running_until(deadline).await?;

        // The supervisor and owner have proved no old execution remains. Only
        // now may old credential metadata be reused (or replaced if unusable).
        let endpoint = admin_probe::register_retired_bootstrap_endpoint(&records[0].spec)?;
        let current = pingap_service_spec(args, endpoint)?;
        anyhow::ensure!(
            admin_probe::proxy_spec_contract_ok(&current),
            "current proxy bootstrap requires absolute runtime paths"
        );
        for record in records {
            self.settle_entry_writes_until(deadline).await?;
            self.require_inert_relative_bootstrap_until(deadline)
                .await?;
            scope.verify_owner_for_cleanup()?;
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            anyhow::ensure!(
                std::fs::read(&record.path)? == record.bytes,
                "historical proxy bootstrap changed before preservation"
            );
            // Generation directories are pruned after successful startup; the
            // untouched predecessor belongs in the stable resident history.
            let archive = current_specification_path(crate::svc_spec::RESIDENT_SPEC_ID)?
                .with_extension(format!("toml.stale-relative-{}", uuid::Uuid::new_v4()));
            preserve_or_replace_until(&archive, record.bytes.clone(), deadline).await?;

            // Keep unrecognized metadata and the wrapper's release identity.
            // Only replace the retired launch fields with this owner's input.
            let old = std::fs::read_to_string(&record.path)?;
            anyhow::ensure!(
                old.as_bytes() == record.bytes,
                "historical proxy bootstrap changed while its original was preserved"
            );
            let mut replacement = current.clone();
            replacement.release_id = record.spec.release_id;
            self.require_inert_relative_bootstrap_until(deadline)
                .await?;
            scope.verify_owner_for_cleanup()?;
            anyhow::ensure!(!cancel.is_cancelled(), "Orchestration cancelled");
            preserve_or_replace_until(
                &record.path,
                bootstrap_bytes(Some(&old), &replacement)?,
                deadline,
            )
            .await?;
            tracing::warn!(
                history = %archive.display(),
                "preserved inert relative proxy bootstrap and installed the current launch contract"
            );
        }
        Ok(())
    }

    pub(super) async fn write_current_bootstrap_until(
        &self,
        args: &RuntimeArgs,
        endpoint: &admin_probe::AdminEndpoint,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let current = pingap_service_spec(args, endpoint)?;
        write_proxy_spec_until(&current, deadline).await
    }

    async fn require_inert_relative_bootstrap_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        if let Some(info) = self.resident_entry_settled_until(deadline).await? {
            let inert = match info.checked_state(PINGAP_PROGRAM)? {
                SupervisorProcessState::Stopped
                | SupervisorProcessState::Exited
                | SupervisorProcessState::Fatal => info.pid == Some(0),
                SupervisorProcessState::Starting
                | SupervisorProcessState::Running
                | SupervisorProcessState::Backoff
                | SupervisorProcessState::Stopping
                | SupervisorProcessState::Unknown => false,
            };
            anyhow::ensure!(
                inert,
                "relative proxy bootstrap still has a live or unconfirmed execution"
            );
        }
        Ok(())
    }
}

/// Replacing the official process must retain forward-compatible metadata as
/// well as the original archive made by relative-bootstrap recovery.
pub(super) async fn write_proxy_spec_until(
    current: &ServiceSpecFile,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let path = current_specification_path(&current.release_id)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read current proxy bootstrap"),
    };
    preserve_or_replace_until(
        &path,
        bootstrap_bytes(existing.as_deref(), current)?,
        deadline,
    )
    .await
}

fn current_specification_path(release: &str) -> Result<PathBuf> {
    let declared = std::path::absolute(crate::svc_spec::spec_root())
        .context("resolve current proxy specification root")?;
    let root = crate::proxy::compiler::canonical_path_with_missing_tail(&declared)?;
    let parent = crate::proxy::compiler::canonical_path_with_missing_tail(&root.join(release))?;
    anyhow::ensure!(
        parent.starts_with(&root),
        "proxy specification destination resolves outside the current specification root"
    );
    Ok(parent.join("pingap.toml"))
}

fn bootstrap_bytes(existing: Option<&str>, current: &ServiceSpecFile) -> Result<Vec<u8>> {
    let mut document: toml::Value = match existing {
        Some(bytes) => {
            toml::from_str(bytes).context("decode proxy bootstrap before replacement")?
        }
        None => toml::Value::Table(Default::default()),
    };
    let fields = toml::Value::try_from(current)?;
    let table = document
        .as_table_mut()
        .context("proxy bootstrap is not a table")?;
    table.extend(
        fields
            .as_table()
            .context("current proxy bootstrap is not a table")?
            .clone(),
    );
    if current.port.is_none() {
        table.remove("port");
    }
    Ok(toml::to_string_pretty(&document)?.into_bytes())
}

async fn preserve_or_replace_until(
    path: &Path,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    crate::xmlrpc::require_deadline_budget("relative proxy bootstrap persistence", deadline)?;
    tokio::time::timeout_at(
        deadline,
        crate::proxy::compiler::replace_file_bytes(path, bytes),
    )
    .await
    .map_err(|_| {
        supervisor::ShutdownUnconfirmed(
            "relative proxy bootstrap persistence has not settled".into(),
        )
    })?
}

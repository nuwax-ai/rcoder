//! Restore a locator for the same retained stop, never for a replacement target.
use super::*;
use crate::{StopMode, StopWorkAttempt};

struct Target<'a> {
    binding: &'a control::Binding,
    request: &'a control::Request,
    supervisor: &'a str,
    generation: Option<&'a str>,
}

struct Evidence {
    original: Option<Vec<u8>>,
    historical: bool,
    _locks: Vec<File>,
}

impl Owner {
    /// Resolve an allocated but never dispatched Stop under the current Owner
    /// lease. Persist this captured attempt before sending any Stop write; a
    /// crash before persistence still means no write was submitted. This uses
    /// the existing offline capture protocol, not a historical PID or target.
    pub fn capture_unresolved_stop_target(&self, attempt: &mut StopWorkAttempt) -> Result<()> {
        ensure!(
            std::fs::canonicalize(&attempt.root)? == self.root
                && attempt.mode == StopMode::Unresolved
                && attempt.captured_generation.is_none()
                && attempt.request.expected_generation.is_none()
                && attempt.request.action == Action::StopWork
                && !attempt.request.request_id.is_empty()
                && attempt.request.request_id.len() <= 128,
            "unresolved stop already contains a captured or invalid target"
        );
        let target = self.capture_offline_target(&attempt.binding)?;
        attempt.mode = StopMode::OfflineChosen {
            supervisor_id: target.supervisor_id,
        };
        attempt.captured_generation = target.generation.clone();
        attempt.request.expected_generation = target.generation;
        Ok(())
    }

    /// Validate a retained Stop before any workspace-wide cleanup. Its captured
    /// generation may still need draining, but another active generation is
    /// never covered by this old request. The caller retains this Owner lease.
    pub fn validate_captured_stop_target(&self, attempt: &StopWorkAttempt) -> Result<()> {
        let target = self.captured_target(attempt)?;
        self.inspect_captured_stop(&target, false)?;
        Ok(())
    }

    /// Rebuild only management discovery after the captured execution and all
    /// local ranges are actually settled. Keep the original request, generation
    /// and supervisor; this projection is not a completed Stop or business result.
    pub fn repair_captured_stop_discovery(&self, attempt: &StopWorkAttempt) -> Result<()> {
        let target = self.captured_target(attempt)?;
        self.repair_captured_discovery(&target)
    }

    fn captured_target<'a>(&self, attempt: &'a StopWorkAttempt) -> Result<Target<'a>> {
        ensure!(
            std::fs::canonicalize(&attempt.root)? == self.root,
            "captured stop belongs to another owner root"
        );
        ensure!(
            attempt.request.expected_generation == attempt.captured_generation,
            "captured stop generation parameters changed"
        );
        let supervisor = match &attempt.mode {
            StopMode::Online { supervisor_id } | StopMode::OfflineChosen { supervisor_id } => {
                supervisor_id.as_str()
            }
            StopMode::Unresolved => anyhow::bail!("stop target has not been captured"),
        };
        Ok(Target {
            binding: &attempt.binding,
            request: &attempt.request,
            supervisor,
            generation: attempt.captured_generation.as_deref(),
        })
    }

    pub(super) fn repair_verified_stop_discovery(
        &self,
        binding: &control::Binding,
        request: &control::Request,
        supervisor: &str,
    ) -> Result<()> {
        let path = self.root.join("supervisor.json");
        if let Ok(discovery) = record::read::<Discovery>(&path)
            && matches!(discovery.version, 1 | control::CONTROL_VERSION)
        {
            // The ordinary verified stop still checks the exact live projection.
            return Ok(());
        }
        self.repair_captured_discovery(&Target {
            binding,
            request,
            supervisor,
            generation: request
                .expected_generation
                .as_deref()
                .filter(|id| !id.is_empty()),
        })
    }

    fn repair_captured_discovery(&self, target: &Target<'_>) -> Result<()> {
        let evidence = self.inspect_captured_stop(target, true)?;
        if !evidence.historical {
            return Ok(());
        }
        let path = self.root.join("supervisor.json");
        let current = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("recheck retained stop locator"),
        };
        ensure!(
            current == evidence.original,
            "captured stop locator changed during repair"
        );
        let mut projection = inactive_discovery(target.binding.clone());
        projection.instance = target.supervisor.to_owned();
        projection.snapshot.supervisor_id = target.supervisor.to_owned();
        projection.snapshot.generation = target.generation.map(str::to_owned);
        projection.snapshot.error = Some(
            "Historical management locator restored after captured execution quiescence; original business outcomes preserved".into(),
        );
        if let Some(bytes) = &evidence.original {
            if let Ok(old) = serde_json::from_slice::<Discovery>(bytes) {
                projection.requests = old.requests;
            }
            preserve_discovery(&path, bytes)?;
        }
        record::save(&path, &projection)?;
        Ok(())
    }

    fn inspect_captured_stop(&self, target: &Target<'_>, settled: bool) -> Result<Evidence> {
        ensure!(
            target.request.action == Action::StopWork
                && !target.request.request_id.is_empty()
                && target.request.request_id.len() <= 128
                && !target.supervisor.is_empty(),
            "invalid captured stop identity"
        );
        ensure!(
            target
                .request
                .expected_generation
                .as_deref()
                .filter(|id| !id.is_empty())
                == target.generation,
            "captured stop generation parameters changed"
        );
        let path = self.root.join("supervisor.json");
        let original = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read captured stop locator"),
        };
        let decoded = original
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<Discovery>(bytes).ok());
        let historical = decoded
            .as_ref()
            .is_none_or(|record| !matches!(record.version, 1 | control::CONTROL_VERSION));
        let raw = original
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok());
        let mut identity_known = false;
        if let Some(raw) = &raw {
            if let Some(binding) = raw.pointer("/snapshot/binding") {
                let recorded: control::Binding = serde_json::from_value(binding.clone())
                    .context("captured stop locator binding is unknown")?;
                ensure!(
                    &recorded == target.binding,
                    "captured stop workspace changed"
                );
            }
            for pointer in ["/instance", "/snapshot/supervisor_id"] {
                if let Some(identity) = raw.pointer(pointer) {
                    ensure!(
                        identity.as_str() == Some(target.supervisor),
                        "captured stop supervisor changed"
                    );
                    identity_known = true;
                }
            }
            if let Some(generation) = raw.pointer("/snapshot/generation") {
                ensure!(
                    (generation.is_null() && target.generation.is_none())
                        || generation
                            .as_str()
                            .is_some_and(|id| target.generation == Some(id)),
                    "captured stop execution generation changed"
                );
            }
        }
        let current_domain = crate::domain::PhysicalDomain::from_env()?;
        let mut locks = Vec::new();
        let mut captured_generation = false;
        let work = self.root.join("work");
        if work.try_exists()? {
            for entry in std::fs::read_dir(work)? {
                let Some(root) = record::generation_entry_root(entry?)? else {
                    continue;
                };
                ensure!(
                    std::fs::canonicalize(&root)? == root,
                    "captured stop work resolves outside owner root"
                );
                locks.push(record::lock(&root.join("generation.lock"))?);
                locks.push(record::lock(&root.join("external-cleanup.lock"))?);
                let value = record::generation(&root)?;
                let captured = target.generation == Some(value.id.as_str());
                if captured {
                    ensure!(
                        value.supervisor == target.supervisor,
                        "captured stop ledger supervisor changed"
                    );
                    captured_generation = true;
                    identity_known = true;
                }
                ensure!(
                    captured && !settled
                        || matches!(
                            value.phase,
                            GenerationPhase::Quiescent | GenerationPhase::Revoked
                        ),
                    "captured stop cannot clean another active or unknown generation"
                );
                if settled || !captured {
                    process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
                    ensure!(
                        process_utils::guardian::is_scope_settled(&root)?,
                        "captured stop command outcome remains unsettled"
                    );
                }
                if captured {
                    ensure!(
                        same_workspace_domain(
                            value.physical_domain.as_ref(),
                            current_domain.as_ref()
                        ),
                        "captured stop generation belongs to another physical workspace"
                    );
                }
            }
        }
        ensure!(
            target.generation.is_none() || captured_generation,
            "captured stop generation ledger is unavailable"
        );
        let resident = self.root.join("resident");
        if resident.try_exists()? {
            for entry in std::fs::read_dir(resident)? {
                let root = entry?.path();
                ensure!(
                    std::fs::canonicalize(&root)? == root,
                    "captured stop resident resolves outside owner root"
                );
                let identity =
                    process_utils::command_authority::read_historical_resident_identity(&root)?;
                ensure!(
                    identity.binding == serde_json::to_value(target.binding)?,
                    "captured stop resident belongs to another workspace"
                );
                let domain = identity
                    .physical_domain
                    .as_ref()
                    .map(|raw| serde_json::from_value::<crate::domain::PhysicalDomain>(raw.clone()))
                    .transpose()?;
                ensure!(
                    same_workspace_domain(domain.as_ref(), current_domain.as_ref()),
                    "captured stop resident belongs to another physical workspace"
                );
                if identity.owner_instance == target.supervisor {
                    identity_known = true;
                }
                if settled {
                    process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
                    ensure!(
                        process_utils::guardian::is_historical_resident_scope_settled(
                            &root, &identity
                        )?,
                        "captured stop resident commands remain unsettled"
                    );
                }
            }
        }
        ensure!(
            identity_known,
            "captured stop identity is not recoverable from its original execution range"
        );
        Ok(Evidence {
            original,
            historical,
            _locks: locks,
        })
    }
}

fn same_workspace_domain(
    old: Option<&crate::domain::PhysicalDomain>,
    current: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    match (old, current) {
        (None, None) => true,
        (Some(old), Some(current)) => {
            old.authority == current.authority && old.volume == current.volume
        }
        _ => false,
    }
}

#[cfg(all(test, unix))]
mod tests;

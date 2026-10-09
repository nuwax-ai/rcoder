//! Bounded retirement of one captured management owner, shared by CLI and platform.
use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};

use crate::{Action, FailureCode, Intent, Phase, Problem, Request, Snapshot};

/// Read the unique completed Shutdown for one explicitly captured supervisor.
/// Return the original request parameters, including the idle condition; never
/// guess the latest operation or manufacture a request from a cleared live slot.
pub fn completed_shutdown_for(
    root: &Path,
    supervisor_id: &str,
) -> Result<Option<(Request, Snapshot)>> {
    let discovery: crate::control::Discovery = crate::record::read(&root.join("supervisor.json"))?;
    ensure!(
        matches!(discovery.version, 1 | crate::control::CONTROL_VERSION),
        "unsupported supervisor receipt"
    );
    ensure!(
        !supervisor_id.is_empty(),
        "completed Shutdown requires a supervisor identity"
    );
    let mut matching = discovery
        .requests
        .into_iter()
        .filter(|(request, snapshot)| {
            request.action == Action::Shutdown
                && snapshot.supervisor_id == supervisor_id
                && snapshot.phase == Phase::Stopped
                && snapshot.intent == Intent::Shutdown
        });
    let Some((request, mut snapshot)) = matching.next() else {
        return Ok(None);
    };
    ensure!(
        matching.next().is_none(),
        "multiple completed Shutdown receipts for the captured supervisor"
    );
    ensure!(
        !request.request_id.is_empty(),
        "completed Shutdown request identity missing"
    );
    if let Some(recorded) = &snapshot.operation_id {
        ensure!(
            recorded == &request.request_id,
            "completed Shutdown receipt request identity mismatch"
        );
    } else {
        // Older terminal snapshots clear the live slot; the ledger key retains
        // the exact request identity, as in saved_request_snapshot.
        snapshot.operation_id = Some(request.request_id.clone());
    }
    Ok(Some((request, snapshot)))
}

/// Retire exactly `before` using one retained Shutdown identity. The caller must
/// establish application, workspace and state authority before calling this.
/// Success requires the exact durable terminal receipt, aggregate generation
/// cleanup, and release of the existing owner lock. No successor is stopped.
pub async fn shutdown_captured_owner(
    root: &Path,
    before: &Snapshot,
    request: &Request,
    budget: Duration,
) -> Result<()> {
    ensure!(
        request.action == Action::Shutdown,
        "owner handover requires Shutdown"
    );
    ensure!(
        request.expected_generation.is_some()
            && request.matches_generation(before.generation.as_deref()),
        "owner handover requires an exactly captured generation, including idle owners"
    );
    // A restoring control owner can drain a generation started by an older
    // supervisor. Capture that generation's immutable execution identity now;
    // never equate it with the current control owner's identity or rediscover it
    // only after cleanup has completed.
    let generation_owner = before
        .generation
        .as_deref()
        .map(|generation| {
            let record = crate::record::generation(&crate::record::work_root(root, generation)?)?;
            ensure!(
                record.id == generation && !record.supervisor.is_empty(),
                "captured execution generation identity is incomplete"
            );
            Ok::<_, anyhow::Error>(record.supervisor)
        })
        .transpose()?;
    tokio::time::timeout(
        budget,
        shutdown_inner(root, before, request, generation_owner.as_deref()),
    )
    .await
    .context("captured owner Shutdown timed out; the same request may be retried")?
}

fn unchanged(before: &Snapshot, observed: &Snapshot) -> Result<()> {
    if observed.supervisor_id != before.supervisor_id
        || observed.binding != before.binding
        || (observed.generation != before.generation
            && !(observed.generation.is_none() && observed.phase == Phase::Stopped))
    {
        return Err(Problem {
            code: FailureCode::IdentityChanged,
            message: "owner identity or generation changed during captured Shutdown".into(),
        }
        .into());
    }
    Ok(())
}

fn receipt(root: &Path, request: &Request) -> Result<Snapshot> {
    match crate::saved_request_snapshot(root, request)? {
        Some(snapshot) => Ok(snapshot),
        None => crate::last_snapshot(root),
    }
}

async fn shutdown_inner(
    root: &Path,
    before: &Snapshot,
    request: &Request,
    generation_owner: Option<&str>,
) -> Result<()> {
    loop {
        let snapshot =
            match crate::control_verified(root, request.clone(), &before.supervisor_id).await {
                Ok(snapshot) => snapshot,
                Err(error)
                    if error
                        .downcast_ref::<Problem>()
                        .is_some_and(|problem| problem.code != FailureCode::Busy) =>
                {
                    return Err(error);
                }
                Err(_) => receipt(root, request)?,
            };
        if snapshot.supervisor_id == before.supervisor_id
            && snapshot.binding == before.binding
            && snapshot.generation.is_none()
            && before.generation.is_some()
            && snapshot.intent == Intent::Shutdown
            && snapshot.operation_id.as_deref() == Some(&request.request_id)
            && matches!(
                snapshot.phase,
                Phase::CleanupPending | Phase::RecoveryRequired
            )
        {
            // Business cleanup may finish before owner resources (resident
            // proxy, pending publications) settle. It is still this exact
            // Shutdown, not a replacement owner/generation. Require positive
            // original-generation cleanup evidence and retain the same request.
            let generation = before
                .generation
                .as_deref()
                .context("captured generation missing")?;
            let proof = crate::verify_local_quiescent(root, generation)?
                .context("owner business cleanup belongs to another process domain")?;
            ensure!(
                proof.generation == generation
                    && Some(proof.supervisor_id.as_str()) == generation_owner,
                "captured business generation cleanup identity differs"
            );
            if snapshot.phase == Phase::RecoveryRequired {
                return Err(snapshot.recovery_error());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        unchanged(before, &snapshot)?;
        if snapshot.operation_id.as_deref() == Some(&request.request_id) {
            if snapshot.phase == Phase::RecoveryRequired {
                return Err(snapshot.recovery_error());
            }
            if snapshot.phase == Phase::Stopped && snapshot.intent == Intent::Shutdown {
                // Hold the same inode while checking durable cleanup. Open only:
                // an absent lock is not permission to manufacture a replacement.
                let released = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(root.join("owner.lock"))
                    .context("open existing owner lock")?;
                match released.try_lock() {
                    Ok(()) => {
                        let live_slot = crate::last_snapshot(root)?;
                        unchanged(before, &live_slot)?;
                        ensure!(
                            live_slot.phase == Phase::Stopped
                                && live_slot.intent == Intent::Shutdown,
                            "owner did not finish the captured Shutdown before releasing its lock"
                        );
                        let terminal = receipt(root, request)?;
                        unchanged(before, &terminal)?;
                        ensure!(
                            terminal.operation_id.as_deref() == Some(&request.request_id)
                                && terminal.phase == Phase::Stopped
                                && terminal.intent == Intent::Shutdown,
                            "captured Shutdown terminal receipt changed before lock release"
                        );
                        if let Some(generation) = before.generation.as_deref() {
                            let proof = crate::verify_local_quiescent(root, generation)?
                                .context("owner cleanup belongs to another process domain")?;
                            if proof.generation != generation
                                || Some(proof.supervisor_id.as_str()) != generation_owner
                            {
                                return Err(Problem {
                                    code: FailureCode::IdentityChanged,
                                    message: "captured execution generation identity changed during Shutdown".into(),
                                }.into());
                            }
                        }
                        return Ok(());
                    }
                    Err(std::fs::TryLockError::WouldBlock) => {}
                    Err(error) => return Err(error).context("verify owner lock release"),
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

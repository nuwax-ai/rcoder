//! Shared control recovery for CLI clients and the platform file service.
use crate::{Action, Binding, Intent, Owner, Phase, Request, Snapshot, control, last_snapshot};
use anyhow::{Context, Result, ensure};
use std::{path::Path, time::Duration};

/// Stop the captured execution tree, keeping/recreating management in Stopped.
/// A completion is tied to this request and generation, not a later status poll.
pub async fn stop_work(root: &Path, binding: &Binding, budget: Duration) -> Result<Snapshot> {
    tokio::time::timeout(budget, async {
        let (before, offline) = match control(root, Request::new(Action::Status)).await {
            Ok(before) => (before, None),
            Err(error) => {
                let owner = Owner::try_acquire(root)?
                    .with_context(|| format!("independent supervisor unavailable: {error:#}"))?;
                (last_snapshot(root)?, Some(owner))
            }
        };
        ensure!(
            &before.binding == binding,
            "supervisor belongs to another workspace"
        );
        let mut request = Request::new(Action::StopWork);
        if let Some(generation) = &before.generation {
            request.request_id = format!("stop-work-{generation}");
        }
        request.expected_generation = before.generation.clone();
        if let Some(owner) = offline {
            let result = owner.stop_offline(&request).await?;
            ensure!(
                result.phase == Phase::Stopped && result.intent == Intent::Stopped,
                "offline stop is not complete"
            );
            if let Some(generation) = &before.generation {
                crate::verify_local_quiescent(root, generation)?;
            }
            return Ok(result);
        }
        // Re-sending this request only observes its own acceptance/terminal receipt.
        loop {
            let result = match control(root, request.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    let saved = last_snapshot(root)?;
                    ensure!(
                        saved.supervisor_id == before.supervisor_id
                            && saved.operation_id.as_deref() == Some(&request.request_id)
                            && saved.phase == Phase::Stopped
                            && saved.intent == Intent::Stopped,
                        "stop response unavailable without matching completion: {error:#}"
                    );
                    saved
                }
            };
            ensure!(
                &result.binding == binding && result.supervisor_id == before.supervisor_id,
                "supervisor changed during stop"
            );
            let completed = (result.phase == Phase::Ready
                && result.intent == Intent::Stopped
                && result.generation != before.generation)
                || (result.phase == Phase::Stopped
                    && result.intent == Intent::Stopped
                    && result.operation_id.as_deref() == Some(&request.request_id));
            if completed {
                if let Some(generation) = &before.generation {
                    crate::verify_local_quiescent(root, generation)?;
                }
                return Ok(result);
            }
            if result.phase == Phase::RecoveryRequired {
                return Err(result.recovery_error());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("stop work and restore management timed out")?
}

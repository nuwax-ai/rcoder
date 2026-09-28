//! Shared control recovery for CLI clients and the platform file service.
use crate::{Action, Binding, Intent, Owner, Phase, Request, Snapshot, control, last_snapshot};
use anyhow::{Context, Result, ensure};
use std::{path::Path, time::Duration};

/// Stop the captured execution tree, keeping/recreating management in Stopped.
/// A completion is tied to this request and generation, not a later status poll.
pub async fn stop_work(root: &Path, binding: &Binding, budget: Duration) -> Result<Snapshot> {
    tokio::time::timeout(budget, async {
        let before = match control(root, Request::new(Action::Status)).await {
            Ok(before) => before,
            Err(error) => {
                let owner = Owner::try_acquire(root)?
                    .with_context(|| format!("independent supervisor unavailable: {error:#}"))?;
                // The stable owner lock plus per-generation cleanup is the
                // authority for offline Stop, even if endpoint discovery is
                // missing or damaged. No cached PID or success is invented.
                let result = owner
                    .stop_offline(binding, &Request::new(Action::StopWork))
                    .await?;
                ensure!(
                    result.phase == Phase::Stopped && result.intent == Intent::Stopped,
                    "offline stop is not complete"
                );
                return Ok(result);
            }
        };
        ensure!(
            &before.binding == binding,
            "supervisor belongs to another workspace"
        );
        let mut request = Request::new(Action::StopWork);
        // Each user attempt gets a fresh identity. Reuse it inside this call
        // for lost responses, but do not permanently replay an earlier failed
        // stop just because it targeted the same generation.
        request.expected_generation = before.generation.clone();
        // Re-sending this request only observes its own acceptance/terminal receipt.
        loop {
            let result = match control(root, request.clone()).await {
                Ok(result) => result,
                // Busy / identity / protocol rejections are actual refusals,
                // not transport failures to hide behind retries.
                Err(error) if error.downcast_ref::<crate::Problem>().is_some() => {
                    return Err(error);
                }
                Err(error) => {
                    let saved = last_snapshot(root)?;
                    ensure!(
                        saved.supervisor_id == before.supervisor_id && &saved.binding == binding,
                        "supervisor changed while stop response was unavailable: {error:#}"
                    );
                    if saved.operation_id.as_deref() == Some(&request.request_id)
                        && saved.phase == Phase::Stopped
                        && saved.intent == Intent::Stopped
                    {
                        saved
                    } else {
                        // A dropped reply may follow acceptance or even a
                        // completed Stop whose slot is already cleared. Ask the
                        // same live owner for this request's receipt, bounded
                        // by the original call deadline; never submit a new ID.
                        crate::record::is_locked(&root.join("owner.lock")).with_context(|| {
                            format!("stop response lost and supervisor exited: {error:#}")
                        })?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Discovery, Envelope, Reply};

    #[tokio::test]
    async fn lost_stop_reply_reuses_request_identity_after_current_slot_was_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let _owner = Owner::try_acquire(&root).unwrap().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let binding = Binding {
            component: "fixture".into(),
            resource: root.clone(),
        };
        let mut discovery = Discovery {
            version: control::CONTROL_VERSION,
            instance: "same-owner".into(),
            address: listener.local_addr().unwrap().to_string(),
            token: "fixture-token".into(),
            requests: Vec::new(),
            snapshot: Snapshot {
                version: 1,
                binding: binding.clone(),
                supervisor_id: "same-owner".into(),
                generation: None,
                phase: Phase::Ready,
                intent: Intent::Run,
                operation_id: None,
                error: None,
                problem: None,
            },
        };
        crate::record::save(&root.join("supervisor.json"), &discovery).unwrap();
        let scope = root.clone();
        let server = tokio::spawn(async move {
            let mut accepted: Option<Request> = None;
            let mut completion = None;
            for turn in 0..3 {
                let (mut stream, _) = listener.accept().await?;
                let envelope: Envelope = control::receive(&mut stream).await?;
                let snapshot = match turn {
                    0 => {
                        ensure!(
                            envelope.request.action == Action::Status,
                            "status must precede Stop"
                        );
                        discovery.snapshot.clone()
                    }
                    1 => {
                        ensure!(envelope.request.action == Action::StopWork, "expected Stop");
                        let mut done = discovery.snapshot.clone();
                        done.generation = Some(uuid::Uuid::new_v4().to_string());
                        done.intent = Intent::Stopped;
                        done.operation_id = Some(envelope.request.request_id.clone());
                        discovery
                            .requests
                            .push((envelope.request.clone(), done.clone()));
                        discovery.snapshot.generation = done.generation.clone();
                        // Current Ready status has already cleared the slot;
                        // only the per-request receipt proves this Stop finished.
                        crate::record::save(&scope.join("supervisor.json"), &discovery)?;
                        accepted = Some(envelope.request);
                        completion = Some(done);
                        continue; // drop the connection AFTER accepting
                    }
                    _ => {
                        ensure!(
                            accepted.as_ref() == Some(&envelope.request),
                            "lost reply created a new Stop"
                        );
                        completion.take().context("completion missing")?
                    }
                };
                control::send(
                    &mut stream,
                    &Reply {
                        instance: discovery.instance.clone(),
                        snapshot,
                        error: None,
                    },
                )
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let result = stop_work(&root, &binding, Duration::from_secs(5)).await;
        if result.is_err() {
            server.abort();
        }
        let result = result.unwrap();
        server.await.unwrap().unwrap();
        assert_eq!(result.phase, Phase::Ready);
        assert_eq!(result.intent, Intent::Stopped);
        assert!(result.operation_id.is_some());
    }
}

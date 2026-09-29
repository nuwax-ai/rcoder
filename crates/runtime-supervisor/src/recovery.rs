//! Shared control recovery for CLI clients and the platform file service.
use crate::{Action, Binding, Intent, Owner, Phase, Request, Snapshot, control, last_snapshot};
use anyhow::{Context, Result, ensure};
use std::{path::Path, time::Duration};

/// A retained, continuable supervision Stop for one captured target.
///
/// The stop identity (request_id, action, expected_generation, captured
/// supervisor/generation) is fixed **before the first Stop write**. A caller
/// whose reply is lost, or whose wait budget expires, resumes the same
/// attempt instead of submitting a second stop that its own in-flight stop
/// would reject as Busy. The caller may serialize the attempt between
/// processes; it never carries credentials.
#[derive(Debug, Clone)]
pub struct StopWorkAttempt {
    pub root: std::path::PathBuf,
    pub binding: Binding,
    /// Supervisor identity captured by the pre-stop Status probe. `None`
    /// until a `continue_stop_work` first establishes online mode; offline
    /// attempts legitimately stay `None`.
    pub captured_supervisor_id: Option<String>,
    /// Generation captured before the first stop write; retries never modify it.
    pub captured_generation: Option<String>,
    pub request: Request,
}

/// Capture the target's current identity and allocate the stop request.
/// Read-only apart from the in-memory attempt: no stop is written yet, so
/// callers may prepare early and continue under a later budget.
pub async fn prepare_stop_work(root: &Path, binding: &Binding) -> Result<StopWorkAttempt> {
    let mut request = Request::new(Action::StopWork);
    let mut captured_supervisor_id = None;
    let mut captured_generation = None;
    if let Ok(before) = control(root, Request::new(Action::Status)).await {
        ensure!(
            &before.binding == binding,
            "supervisor belongs to another workspace"
        );
        captured_supervisor_id = Some(before.supervisor_id);
        captured_generation = before.generation.clone();
        request.expected_generation = captured_generation.clone();
    }
    Ok(StopWorkAttempt {
        root: root.to_path_buf(),
        binding: binding.clone(),
        captured_supervisor_id,
        captured_generation,
        request,
    })
}

/// Continue (or start) the prepared stop within `budget`. Timeouts and lost
/// replies keep the attempt valid: the next call resumes the SAME request.
pub async fn continue_stop_work(
    attempt: &mut StopWorkAttempt,
    budget: Duration,
) -> Result<Snapshot> {
    tokio::time::timeout(budget, continue_stop_work_inner(attempt))
        .await
        .context("stop work and restore management timed out")?
}

async fn continue_stop_work_inner(attempt: &mut StopWorkAttempt) -> Result<Snapshot> {
    let root = attempt.root.clone();
    let binding = attempt.binding.clone();
    // Resolve the mode once: a supervisor captured online stays online for the
    // whole attempt; an offline attempt keeps using offline stop with the same
    // request even if a later continue could reach a (replaced) endpoint.
    if attempt.captured_supervisor_id.is_none() {
        match control(&root, Request::new(Action::Status)).await {
            Ok(before) => {
                ensure!(
                    before.binding == binding,
                    "supervisor belongs to another workspace"
                );
                attempt.captured_supervisor_id = Some(before.supervisor_id);
                attempt.captured_generation = before.generation.clone();
                // Late bind: the first stop write has not happened yet, so
                // fixing expected_generation here still precedes it.
                if attempt.request.expected_generation.is_none() {
                    attempt.request.expected_generation = attempt.captured_generation.clone();
                }
            }
            Err(error) => {
                let owner = Owner::try_acquire(&root)?
                    .with_context(|| format!("independent supervisor unavailable: {error:#}"))?;
                // The stable owner lock plus per-generation cleanup is the
                // authority for offline Stop, even if endpoint discovery is
                // missing or damaged. No cached PID or success is invented.
                let result = owner.stop_offline(&binding, &attempt.request).await?;
                ensure!(
                    result.phase == Phase::Stopped && result.intent == Intent::Stopped,
                    "offline stop is not complete"
                );
                return Ok(result);
            }
        }
    }
    // 模式解析保证：进入在线循环前 captured_supervisor_id 已被设置——
    // None 时上方分支已在离线停止中返回。用模式匹配表达该不变量，不 panic。
    let Some(captured_supervisor_id) = attempt.captured_supervisor_id.clone() else {
        anyhow::bail!("online stop loop reached without a captured supervisor");
    };
    let before_generation = attempt.captured_generation.clone();
    // Re-sending this request only observes its own acceptance/terminal receipt.
    loop {
        let result = match control(&root, attempt.request.clone()).await {
            Ok(result) => result,
            // Busy / identity / protocol rejections are actual refusals,
            // not transport failures to hide behind retries.
            Err(error) if error.downcast_ref::<crate::Problem>().is_some() => {
                return Err(error);
            }
            Err(error) => {
                let saved = last_snapshot(&root)?;
                ensure!(
                    saved.supervisor_id == captured_supervisor_id && saved.binding == binding,
                    "supervisor changed while stop response was unavailable: {error:#}"
                );
                if saved.operation_id.as_deref() == Some(&attempt.request.request_id)
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
            result.binding == binding && result.supervisor_id == captured_supervisor_id,
            "supervisor changed during stop"
        );
        let completed = (result.phase == Phase::Ready
            && result.intent == Intent::Stopped
            && result.generation != before_generation)
            || (result.phase == Phase::Stopped
                && result.intent == Intent::Stopped
                && result.operation_id.as_deref() == Some(&attempt.request.request_id));
        if completed {
            if let Some(generation) = &before_generation {
                crate::verify_local_quiescent(root.as_path(), generation)?;
            }
            return Ok(result);
        }
        if result.phase == Phase::RecoveryRequired {
            return Err(result.recovery_error());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Stop the captured execution tree, keeping/recreating management in Stopped.
/// A completion is tied to this request and generation, not a later status poll.
pub async fn stop_work(root: &Path, binding: &Binding, budget: Duration) -> Result<Snapshot> {
    let mut attempt = prepare_stop_work(root, binding).await?;
    continue_stop_work(&mut attempt, budget).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Discovery, Envelope, Reply};

    /// R4：第一次 continue 超时后，attempt 保留同一 request_id /
    /// expected_generation 续行——监督端绝不看到第二个 Stop 身份。
    #[tokio::test]
    async fn timeout_then_continue_resumes_the_same_stop_request() {
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
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Request>::new()));
        let seen_reply_gate = seen.clone();
        let stop_arrived = std::sync::Arc::new(tokio::sync::Notify::new());
        let stop_waiter = stop_arrived.clone();
        let server = tokio::spawn(async move {
            #[allow(unused_assignments)]
            let mut completion: Option<Snapshot> = None;
            for turn in 0..4 {
                let (mut stream, _) = listener.accept().await?;
                let envelope: Envelope = control::receive(&mut stream).await?;
                let snapshot = match envelope.request.action {
                    Action::Status => discovery.snapshot.clone(),
                    Action::StopWork => {
                        seen_reply_gate
                            .lock()
                            .unwrap()
                            .push(envelope.request.clone());
                        let mut done = discovery.snapshot.clone();
                        done.generation = Some(uuid::Uuid::new_v4().to_string());
                        done.intent = Intent::Stopped;
                        done.operation_id = Some(envelope.request.request_id.clone());
                        discovery
                            .requests
                            .push((envelope.request.clone(), done.clone()));
                        discovery.snapshot.generation = done.generation.clone();
                        crate::record::save(&scope.join("supervisor.json"), &discovery)?;
                        completion = Some(done);
                        match turn {
                            // 首次提交：受理后不回（drop）——调用方预算内必超时。
                            1 => {
                                stop_arrived.notify_one();
                                continue;
                            }
                            // 预算内重发：挂住连接不回（900ms > 首次预算 700ms），
                            // 强制第一次 continue 超时且不产生第三个请求身份。
                            2 => {
                                tokio::time::sleep(Duration::from_millis(900)).await;
                                continue;
                            }
                            _ => completion.take().context("completion missing")?,
                        }
                    }
                    _ => anyhow::bail!("unexpected action"),
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
        let mut attempt = prepare_stop_work(&root, &binding).await.unwrap();
        // First continue: bounded budget, reply deliberately dropped → timeout.
        let first = continue_stop_work(&mut attempt, Duration::from_millis(700)).await;
        if first.is_ok() {
            server.abort();
            panic!("first continue must time out; got {:?}", first);
        }
        tokio::time::timeout(Duration::from_secs(2), stop_waiter.notified())
            .await
            .expect("fixture must have accepted the first stop");
        // Second continue: SAME attempt object, no new identity.
        let result = continue_stop_work(&mut attempt, Duration::from_secs(5))
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        assert_eq!(result.intent, Intent::Stopped);
        // 全部投递（首次受理、预算内重发、续行重发）必须是同一停止身份。
        let seen = seen.lock().unwrap();
        assert!(seen.len() >= 2, "stop deliveries recorded: {seen:?}");
        for delivered in seen.iter() {
            assert_eq!(
                delivered.request_id, attempt.request.request_id,
                "every delivery must reuse the single stop identity"
            );
            assert!(matches!(delivered.action, Action::StopWork));
            assert_eq!(
                delivered.expected_generation,
                attempt.request.expected_generation
            );
        }
    }

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

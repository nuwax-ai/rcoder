//! Shared control recovery for CLI clients and the platform file service.
use crate::{Action, Binding, Intent, Owner, Phase, Request, Snapshot, control, last_snapshot};
use anyhow::{Context, Result};
use std::{path::Path, time::Duration};

/// How a retained stop addresses its target. `Unresolved` is an unbound
/// allocation whose identity late-binds inside the first
/// `continue_stop_work`; once Online or OfflineChosen is set, later continues
/// never re-target a different supervisor at the same root (DEV-R4: a single
/// `Option` used to conflate "not chosen yet" with "offline already chosen",
/// letting a retry bind a replacement owner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopMode {
    Unresolved,
    Online { supervisor_id: String },
    OfflineChosen { supervisor_id: String },
}

/// A retained, continuable supervision Stop for one captured target.
///
/// The stop identity (request_id, action, expected_generation, target
/// root/binding) is fixed **before the first Stop write**. A caller whose
/// reply is lost, or whose wait budget expires, resumes the same attempt
/// instead of submitting a second stop that its own in-flight stop would
/// reject as Busy. The caller may serialize the attempt between processes; it
/// never carries credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopWorkAttempt {
    pub root: std::path::PathBuf,
    pub binding: Binding,
    pub mode: StopMode,
    /// Generation captured before the first stop write; retries never modify it.
    pub captured_generation: Option<String>,
    pub request: Request,
}

impl StopWorkAttempt {
    /// Allocate the stop identity for one target without any I/O. The target
    /// (root + binding) and request identity are complete from here on, so a
    /// caller may persist the attempt before the first stop write and resume
    /// the same identity after a process restart.
    pub fn allocate(root: &Path, binding: &Binding) -> Self {
        Self {
            root: root.to_path_buf(),
            binding: binding.clone(),
            mode: StopMode::Unresolved,
            captured_generation: None,
            request: Request::new(Action::StopWork),
        }
    }
}

/// The target or protocol explicitly refused this stop attempt (Busy,
/// identity change, protocol mismatch, recovery-required terminal state).
/// Distinct from unknown outcomes — timeouts, lost replies, unreadable
/// state — which must keep the original request identity for resumption.
/// An explicit refusal is the only outcome that lets the caller drop the
/// attempt; a fresh user retry may then allocate a new identity.
#[derive(Debug)]
pub struct StopRefused {
    pub detail: String,
}
impl std::fmt::Display for StopRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stop refused: {}", self.detail)
    }
}
impl std::error::Error for StopRefused {}

pub fn is_stop_refused(error: &anyhow::Error) -> bool {
    error.downcast_ref::<StopRefused>().is_some()
        || error.downcast_ref::<crate::RecoveryError>().is_some()
}

fn refused(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(StopRefused {
        detail: detail.into(),
    })
}

/// Capture the target's current identity and allocate the stop request.
/// Read-only apart from the in-memory attempt: no stop is written yet, so
/// callers may prepare early and continue under a later budget.
pub async fn prepare_stop_work(root: &Path, binding: &Binding) -> Result<StopWorkAttempt> {
    let mut attempt = StopWorkAttempt::allocate(root, binding);
    if let Ok(before) = control(root, Request::new(Action::Status)).await {
        if &before.binding != binding {
            return Err(refused(refused_supervisor_mismatch()));
        }
        attempt.mode = StopMode::Online {
            supervisor_id: before.supervisor_id,
        };
        attempt.captured_generation = before.generation.clone();
        attempt.request.expected_generation = attempt.captured_generation.clone();
    }
    Ok(attempt)
}

fn refused_supervisor_mismatch() -> String {
    "supervisor belongs to another workspace".to_string()
}

/// Continue (or start) the prepared stop within `budget`. Timeouts and lost
/// replies keep the attempt valid: the next call resumes the SAME request.
pub async fn continue_stop_work(
    attempt: &mut StopWorkAttempt,
    budget: Duration,
) -> Result<Snapshot> {
    continue_stop_work_with_checkpoint(attempt, budget, |_| Ok(())).await
}

/// Persist the fully captured target before the first Stop write. Returning an
/// error from `checkpoint` prevents dispatch. It is also called on continuations
/// so a durable caller can reject stale copies of an already-replaced request.
pub async fn continue_stop_work_with_checkpoint<F>(
    attempt: &mut StopWorkAttempt,
    budget: Duration,
    checkpoint: F,
) -> Result<Snapshot>
where
    F: FnMut(&StopWorkAttempt) -> Result<()> + Send,
{
    continue_stop_work_with_cleanup(attempt, budget, None, checkpoint).await
}

/// Continue the same captured stop, supplying the owning CLI's external-engine
/// cleanup adapter if the supervisor died before completing cleanup.
pub async fn continue_stop_work_with_cleanup<F>(
    attempt: &mut StopWorkAttempt,
    budget: Duration,
    cleanup: Option<&crate::CleanupCommand>,
    mut checkpoint: F,
) -> Result<Snapshot>
where
    F: FnMut(&StopWorkAttempt) -> Result<()> + Send,
{
    tokio::time::timeout(
        budget,
        continue_stop_work_inner(attempt, cleanup, &mut checkpoint),
    )
    .await
    .context("stop work and restore management timed out")?
}

async fn continue_stop_work_inner<F>(
    attempt: &mut StopWorkAttempt,
    cleanup: Option<&crate::CleanupCommand>,
    checkpoint: &mut F,
) -> Result<Snapshot>
where
    F: FnMut(&StopWorkAttempt) -> Result<()> + Send,
{
    let root = attempt.root.clone();
    let binding = attempt.binding.clone();
    // 模式解析（DEV-R4）：Unresolved 首次探测后固定 Online/Offline；
    // OfflineChosen 续行不再重新探测——不因 owner 换代把离线尝试重新
    // 绑定到替换后的在线目标。
    if attempt.mode == StopMode::Unresolved {
        match control(&root, Request::new(Action::Status)).await {
            Ok(before) => {
                if before.binding != binding {
                    return Err(refused(refused_supervisor_mismatch()));
                }
                attempt.mode = StopMode::Online {
                    supervisor_id: before.supervisor_id,
                };
                // Late bind: the first stop write has not happened yet, so
                // fixing expected_generation here still precedes it.
                if attempt.captured_generation.is_none() {
                    attempt.captured_generation = before.generation.clone();
                    attempt.request.expected_generation = attempt.captured_generation.clone();
                }
            }
            Err(error) if error.downcast_ref::<crate::Problem>().is_some() => {
                return Err(refused(format!("{error:#}")));
            }
            Err(error) => {
                let owner = Owner::try_acquire(&root)?
                    .with_context(|| format!("independent supervisor unavailable: {error:#}"))?;
                // No stop has been sent while the live owner still holds the
                // lock. Keep that attempt unresolved so a recovered control
                // endpoint can be used on the next continuation.
                let target = owner.capture_offline_target(&binding)?;
                attempt.captured_generation = target.generation.clone();
                attempt.request.expected_generation = target.generation;
                attempt.mode = StopMode::OfflineChosen {
                    supervisor_id: target.supervisor_id.clone(),
                };
                checkpoint(attempt)?;
                return offline_stop(
                    owner,
                    &binding,
                    &attempt.request,
                    &target.supervisor_id,
                    cleanup,
                )
                .await;
            }
        }
    }
    checkpoint(attempt)?;
    let StopMode::Online {
        supervisor_id: captured_supervisor_id,
    } = attempt.mode.clone()
    else {
        if let StopMode::OfflineChosen { supervisor_id } = &attempt.mode {
            let owner = Owner::try_acquire(&root)?.with_context(
                || "independent supervisor unavailable: offline stop is not continuable",
            )?;
            return offline_stop(owner, &binding, &attempt.request, supervisor_id, cleanup).await;
        }
        anyhow::bail!("stop attempt mode resolved inconsistently");
    };
    let before_generation = attempt.captured_generation.clone();
    // Re-sending this request only observes its own acceptance/terminal receipt.
    loop {
        let result = match control::control_verified(
            &root,
            attempt.request.clone(),
            &captured_supervisor_id,
        )
        .await
        {
            Ok(result) => result,
            // Busy / identity / protocol rejections are actual refusals,
            // not transport failures to hide behind retries.
            Err(error) if error.downcast_ref::<crate::Problem>().is_some() => {
                return Err(refused(format!("{error:#}")));
            }
            Err(error) => {
                if let Some(receipt) = control::saved_request_snapshot(&root, &attempt.request)?
                    && receipt.supervisor_id == captured_supervisor_id
                    && receipt.binding == binding
                    && receipt.operation_id.as_deref() == Some(attempt.request.request_id.as_str())
                    && receipt.phase == Phase::Stopped
                    && receipt.intent == Intent::Stopped
                {
                    if let Some(generation) = &before_generation {
                        crate::verify_local_quiescent(root.as_path(), generation)?;
                    }
                    return Ok(receipt);
                }
                let saved = last_snapshot(&root)?;
                if saved.supervisor_id != captured_supervisor_id || saved.binding != binding {
                    return Err(refused(format!(
                        "supervisor changed while stop response was unavailable: {error:#}"
                    )));
                }
                if saved.operation_id.as_deref() == Some(&attempt.request.request_id)
                    && saved.phase == Phase::Stopped
                    && saved.intent == Intent::Stopped
                {
                    if let Some(generation) = &before_generation {
                        crate::verify_local_quiescent(root.as_path(), generation)?;
                    }
                    return Ok(saved);
                }
                // 旧 owner 确认退出（owner.lock 可获取）→ 用同一请求经离线
                // 机制收束本代次（DEV-R4：不能永久锁死在"只能联系已死
                // owner"的分支）；锁仍被持有 → owner 活着，重发同一请求。
                match Owner::try_acquire(&root) {
                    Ok(Some(owner)) => {
                        return offline_stop(
                            owner,
                            &binding,
                            &attempt.request,
                            &captured_supervisor_id,
                            cleanup,
                        )
                        .await;
                    }
                    Ok(None) => {}
                    Err(lock_error) => {
                        return Err(lock_error)
                            .with_context(|| format!("stop response lost: {error:#}"));
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if result.binding != binding || result.supervisor_id != captured_supervisor_id {
            return Err(refused("supervisor changed during stop"));
        }
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
            // 类型化终局错误（wire 契约）：RecoveryRequired 是明确拒绝——
            // 调用方可凭此丢弃 attempt（is_stop_refused 涵盖），新重试可
            // 产生新身份；不是超时/丢回复类未知结果。
            return Err(result.recovery_error());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Offline stop under the acquired owner lock: replays the same request
/// receipt (or records it) and requires a complete Stopped terminal.
async fn offline_stop(
    owner: Owner,
    binding: &Binding,
    request: &Request,
    supervisor_id: &str,
    cleanup: Option<&crate::CleanupCommand>,
) -> Result<Snapshot> {
    let result = owner
        .stop_offline_verified(binding, request, supervisor_id, cleanup)
        .await
        .map_err(|error| {
            if error.downcast_ref::<crate::Problem>().is_some() {
                refused(format!("{error:#}"))
            } else {
                error
            }
        })?;
    if result.phase != Phase::Stopped || result.intent != Intent::Stopped {
        return Err(refused(format!(
            "offline stop is not complete: {}",
            result.diagnostic()
        )));
    }
    Ok(result)
}

/// Stop the captured execution tree, keeping/recreating management in Stopped.
/// A completion is tied to this request and generation, not a later status poll.
pub async fn stop_work(root: &Path, binding: &Binding, budget: Duration) -> Result<Snapshot> {
    let mut attempt = prepare_stop_work(root, binding).await?;
    continue_stop_work(&mut attempt, budget).await
}

/// Stop with the trusted CLI adapter needed to finish abandoned external work.
pub async fn stop_work_with_cleanup(
    root: &Path,
    binding: &Binding,
    budget: Duration,
    cleanup: &crate::CleanupCommand,
) -> Result<Snapshot> {
    let mut attempt = prepare_stop_work(root, binding).await?;
    continue_stop_work_with_cleanup(&mut attempt, budget, Some(cleanup), |_| Ok(())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Discovery, Envelope, Reply};
    use anyhow::ensure;

    fn fixture_binding(root: &Path) -> Binding {
        Binding {
            component: "app-cli".into(),
            resource: root.to_path_buf(),
        }
    }

    fn fixture_discovery(
        root: &Path,
        listener: &tokio::net::TcpListener,
        instance: &str,
    ) -> Discovery {
        Discovery {
            version: control::CONTROL_VERSION,
            instance: instance.into(),
            address: listener.local_addr().unwrap().to_string(),
            token: "fixture-token".into(),
            requests: Vec::new(),
            snapshot: Snapshot {
                version: 1,
                binding: fixture_binding(root),
                supervisor_id: instance.into(),
                generation: None,
                phase: Phase::Ready,
                intent: Intent::Run,
                operation_id: None,
                error: None,
                problem: None,
            },
        }
    }

    #[tokio::test]
    async fn captured_stop_is_checkpointed_before_dispatch_and_survives_caller_loss() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _owner = Owner::try_acquire(root).unwrap().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let discovery = fixture_discovery(root, &listener, "owner-a");
        crate::record::save(&root.join("supervisor.json"), &discovery).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let envelope: Envelope = control::receive(&mut stream).await.unwrap();
            assert_eq!(envelope.request.action, Action::Status);
            control::send(
                &mut stream,
                &Reply {
                    instance: discovery.instance.clone(),
                    snapshot: discovery.snapshot,
                    error: None,
                },
            )
            .await
            .unwrap();
            listener
        });
        let mut attempt = StopWorkAttempt::allocate(root, &fixture_binding(root));
        let mut persisted = None;
        let error =
            continue_stop_work_with_checkpoint(&mut attempt, Duration::from_secs(2), |captured| {
                persisted = Some(captured.clone());
                anyhow::bail!("simulated caller exit after durable checkpoint")
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("simulated caller exit"));
        let listener = server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "Stop must not be sent when checkpoint fails"
        );
        let mut restored = persisted.unwrap();
        assert!(
            matches!(&restored.mode, StopMode::Online { supervisor_id } if supervisor_id == "owner-a")
        );
        let replacement = fixture_discovery(root, &listener, "owner-b");
        crate::record::save(&root.join("supervisor.json"), &replacement).unwrap();
        let error = continue_stop_work(&mut restored, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(is_stop_refused(&error));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "old Stop must not be dispatched to a replacement owner"
        );
    }

    #[tokio::test]
    async fn offline_checkpoint_does_not_authorize_stopping_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let discovery = fixture_discovery(root, &listener, "owner-a");
        crate::record::save(&root.join("supervisor.json"), &discovery).unwrap();
        drop(listener);
        let mut attempt = StopWorkAttempt::allocate(root, &fixture_binding(root));
        let mut persisted = None;
        continue_stop_work_with_checkpoint(&mut attempt, Duration::from_secs(2), |captured| {
            persisted = Some(captured.clone());
            anyhow::bail!("simulated caller exit before offline cleanup")
        })
        .await
        .unwrap_err();
        assert_eq!(last_snapshot(root).unwrap().phase, Phase::Ready);
        let mut restored = persisted.unwrap();
        assert!(
            matches!(&restored.mode, StopMode::OfflineChosen { supervisor_id } if supervisor_id == "owner-a")
        );
        let mut replacement = discovery;
        replacement.instance = "owner-b".into();
        replacement.snapshot.supervisor_id = "owner-b".into();
        crate::record::save(&root.join("supervisor.json"), &replacement).unwrap();
        let before = std::fs::read(root.join("supervisor.json")).unwrap();
        let error = continue_stop_work(&mut restored, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(is_stop_refused(&error));
        assert_eq!(std::fs::read(root.join("supervisor.json")).unwrap(), before);
        // The rejection must not strand the current owner: a fresh explicit
        // stop can capture it and complete with its own receipt.
        assert_eq!(
            stop_work(root, &fixture_binding(root), Duration::from_secs(2))
                .await
                .unwrap()
                .phase,
            Phase::Stopped
        );
    }

    #[tokio::test]
    async fn unavailable_control_with_live_owner_can_retry_online() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _owner = Owner::try_acquire(root).unwrap().unwrap();
        let binding = fixture_binding(root);
        let mut attempt = StopWorkAttempt::allocate(root, &binding);
        let request_id = attempt.request.request_id.clone();

        // The owner holds its lock but has not published its control endpoint.
        continue_stop_work(&mut attempt, Duration::from_secs(2))
            .await
            .expect_err("no control endpoint is available yet");
        let mode_after_failure = attempt.mode.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let discovery = fixture_discovery(root, &listener, "same-owner");
        crate::record::save(&root.join("supervisor.json"), &discovery).unwrap();
        let expected_request = request_id.clone();
        let server = tokio::spawn(async move {
            for action in [Action::Status, Action::StopWork] {
                let (mut stream, _) = listener.accept().await?;
                let envelope: Envelope = control::receive(&mut stream).await?;
                ensure!(envelope.request.action == action, "unexpected action");
                let mut snapshot = discovery.snapshot.clone();
                if action == Action::StopWork {
                    ensure!(
                        envelope.request.request_id == expected_request,
                        "new request"
                    );
                    snapshot.phase = Phase::Stopped;
                    snapshot.intent = Intent::Stopped;
                    snapshot.operation_id = Some(expected_request.clone());
                }
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
            anyhow::Ok(())
        });
        let outcome = continue_stop_work(&mut attempt, Duration::from_secs(2)).await;
        if outcome.is_err() {
            server.abort();
        }
        assert_eq!(mode_after_failure, StopMode::Unresolved);
        assert_eq!(outcome.unwrap().operation_id, Some(request_id));
        server.await.unwrap().unwrap();
    }

    /// R4：第一次 continue 超时后，attempt 保留同一 request_id /
    /// expected_generation 续行——监督端绝不看到第二个 Stop 身份。
    #[tokio::test]
    async fn timeout_then_continue_resumes_the_same_stop_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let _owner = Owner::try_acquire(&root).unwrap().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let binding = fixture_binding(&root);
        let mut discovery = fixture_discovery(&root, &listener, "same-owner");
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
        tokio::time::timeout(Duration::from_secs(8), stop_waiter.notified())
            .await
            .expect("fixture must have accepted the first stop");
        // Second continue: SAME attempt object, no new identity.（预算 15s：
        // 并行测试争抢 CPU 时 tokio 调度延迟会放大重发轮次间隔；核心断言
        // 是"首个 700ms 预算必超时 + 全部投递同一身份"，不受此预算影响。）
        let result = continue_stop_work(&mut attempt, Duration::from_secs(15))
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
        let binding = fixture_binding(&root);
        let mut discovery = fixture_discovery(&root, &listener, "same-owner");
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

    /// R4 反例：prepare 捕获 owner A（generation=None）后 A 被替换为 B
    ///（同根、同 binding、generation 同为 None）。续行必须在**发包前**核对
    /// discovery.instance——绝不把 A 的停止请求投递给 B；结果为明确拒绝
    ///（StopRefused），不是未知错误。
    #[tokio::test]
    async fn replaced_owner_with_null_generation_is_refused_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let _owner = Owner::try_acquire(&root).unwrap().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let binding = fixture_binding(&root);
        let discovery = fixture_discovery(&root, &listener, "owner-a");
        crate::record::save(&root.join("supervisor.json"), &discovery).unwrap();
        // Owner A 应答 Status 后立刻消失（不再 accept），模拟换代。
        let a = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let envelope: Envelope = control::receive(&mut stream).await?;
            ensure!(envelope.request.action == Action::Status, "expected Status");
            control::send(
                &mut stream,
                &Reply {
                    instance: "owner-a".into(),
                    snapshot: discovery.snapshot.clone(),
                    error: None,
                },
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        });
        let mut attempt = prepare_stop_work(&root, &binding).await.unwrap();
        a.await.unwrap().unwrap();
        assert_eq!(
            attempt.mode,
            StopMode::Online {
                supervisor_id: "owner-a".into()
            }
        );
        // 换代：B 重写 discovery（instance=owner-b，generation 仍 None）并接管。
        let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_seen_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let b_seen = b_seen_stop.clone();
        let discovery_b = fixture_discovery(&root, &listener_b, "owner-b");
        crate::record::save(&root.join("supervisor.json"), &discovery_b).unwrap();
        let b = tokio::spawn(async move {
            let (mut stream, _) = listener_b.accept().await?;
            let envelope: Envelope = control::receive(&mut stream).await?;
            if matches!(envelope.request.action, Action::StopWork) {
                b_seen.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok::<_, anyhow::Error>(())
        });
        let refused = continue_stop_work(&mut attempt, Duration::from_secs(5))
            .await
            .expect_err("a replaced owner must refuse the captured stop");
        assert!(
            is_stop_refused(&refused),
            "explicit refusal expected, got: {refused:#}"
        );
        // B 只会被 contact 一次用于确认其不应收到 Stop——发包前拒绝意味着
        // 连这次也不应发生；等待极短窗口后断言无 StopWork 抵达。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !b_seen_stop.load(std::sync::atomic::Ordering::SeqCst),
            "replacement owner must never receive the old stop request"
        );
        b.abort();
    }
}

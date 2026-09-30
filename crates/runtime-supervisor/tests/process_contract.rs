#![cfg(feature = "test-support")]
use anyhow::{Context, Result, ensure};
use runtime_supervisor::{Action, Request, Snapshot, control};
use std::{path::Path, time::Duration};

async fn status(root: &Path) -> Result<Snapshot> {
    control(root, Request::new(Action::Status)).await
}
async fn until(root: &Path, predicate: impl Fn(&Snapshot) -> bool) -> Result<Snapshot> {
    let mut last = String::from("no observation");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match status(root).await {
                Ok(value) if predicate(&value) => return value,
                Ok(value) => last = format!("{value:?}"),
                Err(error) => last = format!("{error:#}"),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("supervisor did not reach expected state; last observation: {last}"))
}
fn start(root: &Path) -> Result<tokio::process::Child> {
    Ok(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_supervision-fixture"))
            .arg(root)
            .spawn()?,
    )
}
async fn shutdown(root: &Path, child: &mut tokio::process::Child) {
    drop(control(root, Request::new(Action::Shutdown)).await);
    if tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .is_err()
    {
        drop(child.kill().await);
    }
}
async fn leaf(root: &Path) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(address) = std::fs::read_to_string(root.join("leaf-address"))
                && tokio::net::TcpStream::connect(&address).await.is_ok()
            {
                return address;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("owned command did not listen")
}

#[cfg(unix)]
async fn paused_cleanup_guardian(root: &Path) -> Result<u32> {
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(root.join("cleanup-parent"))
                && let Ok(pid) = pid.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "cleanup barrier was not reached: {:?}",
            runtime_supervisor::last_snapshot(root)
        )
    })?;
    ensure!(pid > 1, "invalid captured fixture guardian PID");
    Ok(pid)
}

#[cfg(unix)]
async fn signal_fixture(pid: u32, signal: &str) -> Result<()> {
    ensure!(pid > 1, "invalid fixture PID");
    let status = tokio::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .await?;
    ensure!(status.success(), "fixture signal failed");
    Ok(())
}

#[tokio::test]
async fn replacement_container_does_not_replay_stale_shutdown_or_block_new_stop() {
    use runtime_supervisor::{Binding, Intent, Phase, domain::PhysicalDomain};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let work = root.join("work").join(&id);
    std::fs::create_dir_all(work.join("commands")).unwrap();
    let previous = PhysicalDomain {
        authority: "test-runtime".into(),
        instance_source_env: None,
        instance: "old-container".into(),
        volume: "workspace-volume".into(),
    };
    let generation = serde_json::to_vec(&serde_json::json!({
        "version": 1, "id": id, "supervisor": "previous-supervisor", "token": "test",
        "intent": "shutdown", "phase": "Running", "exit_code": null, "error": null,
        "physical_domain": previous
    }))
    .unwrap();
    std::fs::write(work.join("generation.json"), &generation).unwrap();
    let mut interrupted = Request::new(Action::Shutdown);
    interrupted.expected_generation = Some(id.clone());
    let snapshot = Snapshot {
        version: 1,
        binding: Binding {
            component: "runtime".into(),
            resource: root.clone(),
        },
        supervisor_id: "previous-supervisor".into(),
        generation: Some(id),
        phase: Phase::Stopping,
        intent: Intent::Shutdown,
        operation_id: Some(interrupted.request_id.clone()),
        error: None,
        problem: None,
    };
    std::fs::write(
        root.join("supervisor.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 2, "instance": "previous-supervisor", "address": "127.0.0.1:1",
            "token": "test", "snapshot": snapshot, "requests": [[interrupted, snapshot]]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut current = previous;
    current.instance = "replacement-container".into();
    let mut owner = tokio::process::Command::new(env!("CARGO_BIN_EXE_supervision-fixture"))
        .arg(&root)
        .env("SUPERVISION_FIXTURE_ONE_SHOT", "fresh-platform-deployment")
        .env(
            runtime_supervisor::domain::DOMAIN_ENV,
            serde_json::to_string(&current).unwrap(),
        )
        .spawn()
        .unwrap();
    let result = async {
        let ready = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        ensure!(
            std::fs::read_to_string(root.join("one-shot-env-present"))? == "true",
            "replacement stripped the fresh platform deployment input"
        );
        ensure!(
            ready.operation_id.is_none(),
            "old control still occupies current management"
        );
        let old = control(&root, interrupted).await?;
        ensure!(
            old.phase == Phase::RecoveryRequired,
            "old unknown result was changed to success"
        );
        let stop = Request::new(Action::StopWork);
        control(&root, stop.clone()).await?;
        until(&root, |s| {
            s.phase == Phase::Ready && s.generation != ready.generation
        })
        .await?;
        ensure!(
            control(&root, stop).await?.phase == Phase::Ready,
            "current stop did not complete"
        );
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "current business survived stop"
        );
        ensure!(
            std::fs::read(work.join("generation.json"))? == generation,
            "local recovery rewrote a foreign container's cleanup history"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[tokio::test]
async fn pod_uid_stamp_is_persisted_and_used_by_the_next_launch() {
    use runtime_supervisor::{Phase, domain::DOMAIN_ENV};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let domain = serde_json::json!({
        "authority":"test-runtime", "volume":"workspace-volume", "instance":"",
        "instance_source_env":"RCODER_PHYSICAL_POD_UID"
    })
    .to_string();
    let spawn = |uid: &str| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_supervision-fixture"))
            .arg(root)
            .env(DOMAIN_ENV, &domain)
            .env("RCODER_PHYSICAL_POD_UID", uid)
            .env("SUPERVISION_FIXTURE_ONE_SHOT", "fresh-deployment")
            .spawn()
            .unwrap()
    };
    let mut first = spawn("pod-first");
    let result = async {
        let ready = until(root, |s| s.phase == Phase::Ready).await?;
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(
            root.join("work")
                .join(ready.generation.context("generation missing")?)
                .join("generation.json"),
        )?)?;
        ensure!(
            receipt["physical_domain"]["instance"] == "pod-first",
            "stamp not resolved"
        );
        ensure!(
            receipt["physical_domain"]["instance_source_env"] == "RCODER_PHYSICAL_POD_UID",
            "source lost"
        );
        ensure!(receipt["process_epoch"].is_string(), "epoch not persisted");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(root, &mut first).await;
    result.unwrap();
    // Simulate interrupted discovery, using a real persisted worker receipt.
    // Both launches run on this host with the same epoch: only the Pod stamp
    // can classify the previous generation as belonging to another container.
    let path = root.join("supervisor.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["snapshot"]["phase"] = serde_json::json!(Phase::RecoveryRequired);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let mut second = spawn("pod-second");
    let result = async {
        let ready = until(root, |s| s.phase == Phase::Ready).await?;
        ensure!(
            std::fs::read_to_string(root.join("one-shot-env-present"))? == "true",
            "next Pod lost its fresh deployment declaration"
        );
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(
            root.join("work")
                .join(ready.generation.context("generation missing")?)
                .join("generation.json"),
        )?)?;
        ensure!(
            receipt["physical_domain"]["instance"] == "pod-second",
            "stale Pod stamp"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(root, &mut second).await;
    result.unwrap();
}

#[tokio::test]
async fn hung_control_is_stoppable_retries_replay_and_business_stays_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("工作区 with spaces");
    std::fs::create_dir(&root).unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let old = until(&root, |s| s.phase == runtime_supervisor::Phase::Ready).await?;
        let address = leaf(&root).await?;
        let work = root
            .join("work")
            .join(old.generation.as_deref().context("generation missing")?);
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(work.join("generation.json"))?)?;
        let unauthorized = tokio::process::Command::new(env!("CARGO_BIN_EXE_supervision-fixture"))
            .arg(&root)
            .env(runtime_supervisor::WORKER_ENV, &work)
            .env(
                runtime_supervisor::TOKEN_ENV,
                receipt["token"].as_str().context("token missing")?,
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        ensure!(
            !unauthorized.success(),
            "copied authorization launched a second worker"
        );
        std::fs::write(root.join("hang"), "hang actual control path")?;
        let mut request = Request::new(Action::StopWork);
        request.expected_generation = old.generation.clone();
        let accepted = control(&root, request.clone()).await?;
        ensure!(
            accepted.operation_id.as_deref() == Some(&request.request_id),
            "stop identity lost"
        );
        let busy = control(&root, Request::new(Action::Recover))
            .await
            .unwrap_err();
        ensure!(
            busy.downcast_ref::<runtime_supervisor::Problem>()
                .is_some_and(|p| p.code == runtime_supervisor::FailureCode::Busy),
            "conflicting request must return typed Busy, not queue or opaque text"
        );
        let blocker = busy
            .downcast_ref::<runtime_supervisor::RecoveryError>()
            .context("Busy must retain the structured blocker snapshot")?;
        ensure!(
            blocker.snapshot.generation == old.generation
                && blocker.snapshot.operation_id == accepted.operation_id
                && blocker.snapshot.supervisor_id == old.supervisor_id,
            "Busy lost the original generation or operation identity"
        );
        ensure!(
            control(&root, request.clone()).await?.operation_id == accepted.operation_id,
            "retry did not replay"
        );
        let mut conflict = request.clone();
        conflict.action = Action::Recover;
        ensure!(
            control(&root, conflict).await.is_err(),
            "request id allowed parameter replacement"
        );
        until(&root, |s| s.generation != old.generation).await?;
        std::fs::remove_file(root.join("hang"))?;
        until(&root, |s| s.phase == runtime_supervisor::Phase::Ready).await?;
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "old owned command survived Stop"
        );
        runtime_supervisor::verify_quiescent(
            &root,
            old.generation
                .as_deref()
                .context("old generation missing")?,
        )?;
        ensure!(
            control(&root, request).await?.phase == runtime_supervisor::Phase::Ready,
            "completed retry lost terminal result"
        );
        let stopped = status(&root).await?;
        let recover = control(&root, Request::new(Action::Recover)).await?;
        ensure!(
            recover.phase == runtime_supervisor::Phase::Stopping,
            "recover not admitted"
        );
        until(&root, |s| {
            s.phase == runtime_supervisor::Phase::Ready && s.generation != stopped.generation
        })
        .await?;
        let latest_address = leaf(&root).await?;
        // A responsive worker may acknowledge Stop but keep running and ask for
        // two minutes. Interactive policy must keep its own force-stop deadline.
        std::fs::write(root.join("slow-shutdown"), "acknowledge without exiting")?;
        let current = status(&root).await?;
        let start = std::time::Instant::now();
        let done =
            runtime_supervisor::stop_work(&root, &current.binding, Duration::from_secs(8)).await?;
        ensure!(
            start.elapsed() < Duration::from_secs(5),
            "worker extended the stop grace"
        );
        ensure!(
            done.generation != current.generation,
            "independent stop did not replace the worker"
        );
        ensure!(
            tokio::net::TcpStream::connect(&latest_address)
                .await
                .is_err(),
            "owned work survived stop"
        );
        std::fs::remove_file(root.join("slow-shutdown"))?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[tokio::test]
async fn parent_death_drains_hung_worker_and_allows_verified_successor() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("scope");
    std::fs::create_dir(&root).unwrap();
    let mut parent = start(&root).unwrap();
    let result = async {
        let original = until(&root, |s| s.phase == runtime_supervisor::Phase::Ready).await?;
        let address = leaf(&root).await?;
        std::fs::write(root.join("hang"), "unresponsive worker")?;
        let mut stop = Request::new(Action::StopWork);
        stop.expected_generation = original.generation.clone();
        control(&root, stop.clone()).await?;
        parent.kill().await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if runtime_supervisor::verify_quiescent(
                    &root,
                    original
                        .generation
                        .as_deref()
                        .context("missing generation")?,
                )
                .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "worker command survived parent death"
        );
        std::fs::remove_file(root.join("hang"))?;
        parent = start(&root)?;
        let next = until(&root, |s| s.phase == runtime_supervisor::Phase::Ready).await?;
        ensure!(
            next.supervisor_id != original.supervisor_id && next.generation != original.generation,
            "successor reused identity"
        );
        ensure!(
            control(&root, stop).await?.phase == runtime_supervisor::Phase::Ready,
            "accepted Stop was lost across supervisor death"
        );
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "pending Stop restarted business"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(&root, &mut parent).await;
    result.unwrap();
}

/// Real worker exit followed by loss of the guardian before its aggregate
/// receipt. The OS/container epoch does NOT change during this recovery.
#[cfg(unix)]
#[tokio::test]
async fn dead_guardian_after_worker_exit_recovers_in_same_process_space() {
    use runtime_supervisor::{Intent, Phase};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("external-cleanup-enabled"), "enabled").unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let before = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        std::fs::write(root.join("pause-cleanup"), "barrier")?;
        let mut stop = Request::new(Action::StopWork);
        stop.expected_generation = before.generation.clone();
        control(&root, stop.clone()).await?;
        signal_fixture(paused_cleanup_guardian(&root).await?, "-KILL").await?;
        // An old callback retains its guard even after the guardian dies.
        // Recovery remains queryable and cannot start a successor yet.
        until(&root, |s| s.phase == Phase::CleanupPending).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        ensure!(
            status(&root).await?.generation == before.generation,
            "cleanup was bypassed"
        );
        std::fs::remove_file(root.join("pause-cleanup"))?;
        until(&root, |s| {
            s.phase == Phase::Ready && s.generation != before.generation
        })
        .await?;
        runtime_supervisor::verify_quiescent(
            &root,
            before.generation.as_deref().context("generation missing")?,
        )?;
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "old business survived"
        );
        let stopped = control(&root, stop.clone()).await?;
        ensure!(
            stopped.phase == Phase::Ready && stopped.intent == Intent::Stopped,
            "original stop did not finish"
        );
        // A fresh explicit start can run after recovery. A late old stop remains a replay.
        shutdown(&root, &mut owner).await;
        owner = start(&root)?;
        until(&root, |s| s.phase == Phase::Ready).await?;
        let next = leaf(&root).await?;
        control(&root, stop).await?;
        ensure!(
            tokio::net::TcpStream::connect(&next).await.is_ok(),
            "late stop hit successor"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(std::fs::remove_file(root.join("pause-cleanup")));
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn abandoned_cleanup_preserves_one_shot_exit_policy() {
    use runtime_supervisor::Phase;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("external-cleanup-enabled"), "enabled").unwrap();
    std::fs::write(root.join("one-shot-owner"), "enabled").unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let before = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        std::fs::write(root.join("pause-cleanup"), "barrier")?;
        let mut stop = Request::new(Action::StopWork);
        stop.expected_generation = before.generation.clone();
        control(&root, stop).await?;
        signal_fixture(paused_cleanup_guardian(&root).await?, "-KILL").await?;
        std::fs::remove_file(root.join("pause-cleanup"))?;
        ensure!(
            tokio::time::timeout(Duration::from_secs(15), owner.wait())
                .await??
                .success(),
            "one-shot owner did not finish the accepted stop"
        );
        let done = runtime_supervisor::last_snapshot(&root)?;
        ensure!(
            done.phase == Phase::Stopped && done.generation == before.generation,
            "cleanup restarted a one-shot owner"
        );
        runtime_supervisor::verify_quiescent(
            &root,
            before.generation.as_deref().context("generation")?,
        )?;
        ensure!(
            tokio::net::TcpStream::connect(address).await.is_err(),
            "business survived stop"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(std::fs::remove_file(root.join("pause-cleanup")));
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn offline_abandoned_cleanup_retries_same_stop_and_preserves_successor() {
    use runtime_supervisor::{Binding, CleanupCommand, Phase};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("external-cleanup-enabled"), "enabled").unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let before = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        std::fs::write(root.join("pause-cleanup"), "barrier")?;
        let binding = Binding {
            component: "runtime".into(),
            resource: root.clone(),
        };
        let mut attempt = runtime_supervisor::prepare_stop_work(&root, &binding).await?;
        control(&root, attempt.request.clone()).await?;
        let guardian = paused_cleanup_guardian(&root).await?;
        owner.kill().await?;
        signal_fixture(guardian, "-KILL").await?;
        let command = CleanupCommand {
            program: env!("CARGO_BIN_EXE_supervision-fixture").into(),
            args: vec!["--cleanup".into(), root.clone().into_os_string()],
            cwd: root.clone(),
        };
        std::fs::write(root.join("fail-cleanup"), "temporary engine failure")?;
        std::fs::remove_file(root.join("pause-cleanup"))?;
        let first = runtime_supervisor::continue_stop_work_with_cleanup(
            &mut attempt,
            Duration::from_secs(5),
            Some(&command),
            |_| Ok(()),
        )
        .await;
        ensure!(
            first.is_err(),
            "failed engine cleanup was declared complete"
        );
        ensure!(
            runtime_supervisor::verify_quiescent(
                &root,
                before.generation.as_deref().context("generation")?
            )
            .is_err(),
            "early completion receipt"
        );
        std::fs::remove_file(root.join("fail-cleanup"))?;
        let done = runtime_supervisor::continue_stop_work_with_cleanup(
            &mut attempt,
            Duration::from_secs(5),
            Some(&command),
            |_| Ok(()),
        )
        .await?;
        ensure!(
            done.phase == Phase::Stopped
                && done.operation_id.as_deref() == Some(&attempt.request.request_id),
            "stop identity changed"
        );
        ensure!(
            tokio::net::TcpStream::connect(address).await.is_err(),
            "stopped work survived"
        );
        owner = start(&root)?;
        until(&root, |s| s.phase == Phase::Ready).await?;
        // Offline Stop restores management with business autostart suppressed.
        shutdown(&root, &mut owner).await;
        owner = start(&root)?;
        until(&root, |s| s.phase == Phase::Ready).await?;
        let next = leaf(&root).await?;
        let late = runtime_supervisor::continue_stop_work_with_cleanup(
            &mut attempt,
            Duration::from_secs(3),
            Some(&command),
            |_| Ok(()),
        )
        .await;
        ensure!(late.is_err(), "retained stop rebound to successor");
        ensure!(
            tokio::net::TcpStream::connect(next).await.is_ok(),
            "late stop killed successor"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(std::fs::remove_file(root.join("pause-cleanup")));
    drop(std::fs::remove_file(root.join("fail-cleanup")));
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn guardian_term_finishes_cleanup_before_restarting_worker() {
    use runtime_supervisor::Phase;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("external-cleanup-enabled"), "enabled").unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let before = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        let generation = before.generation.as_deref().context("generation")?;
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(
            root.join("work").join(generation).join("generation.json"),
        )?)?;
        let pid = value["worker_pid"].as_u64().context("worker PID")?;
        let parent = tokio::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .await?;
        ensure!(parent.status.success(), "fixture guardian lookup failed");
        let guardian = String::from_utf8(parent.stdout)?.trim().parse::<u32>()?;
        ensure!(
            Some(guardian) != owner.id(),
            "captured owner instead of guardian"
        );
        signal_fixture(guardian, "-TERM").await?;
        until(&root, |s| {
            s.phase == Phase::Ready && s.generation != before.generation
        })
        .await?;
        runtime_supervisor::verify_quiescent(&root, generation)?;
        ensure!(
            tokio::net::TcpStream::connect(address).await.is_err(),
            "TERM left old business running"
        );
        leaf(&root).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        eprintln!(
            "TERM fixture: owner exit={:?}, snapshot={:?}",
            owner.try_wait(),
            runtime_supervisor::last_snapshot(&root)
        );
    }
    shutdown(&root, &mut owner).await;
    result.unwrap();
}

#[tokio::test]
async fn damaged_discovery_and_completed_command_history_do_not_disable_controls() {
    use runtime_supervisor::{Intent, Phase};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let mut parent = start(&root).unwrap();
    let result = async {
        let original = until(&root, |s| s.phase == Phase::Ready).await?;
        let address = leaf(&root).await?;
        let broken = b"{interrupted-discovery";
        std::fs::write(root.join("supervisor.json"), broken)?;
        let repaired = until(&root, |s| s.phase == Phase::Ready).await?;
        ensure!(
            repaired.supervisor_id == original.supervisor_id
                && repaired.generation == original.generation
                && tokio::net::TcpStream::connect(&address).await.is_ok(),
            "live discovery repair replaced running work"
        );
        ensure!(
            runtime_supervisor::Owner::try_acquire(&root)?.is_none(),
            "discovery repair released ownership"
        );
        let backups: Vec<_> = std::fs::read_dir(&root)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("supervisor.corrupt-")
            })
            .collect();
        ensure!(
            backups.len() == 1 && std::fs::read(backups[0].path())? == broken,
            "original damaged discovery was not preserved"
        );

        let stopped =
            runtime_supervisor::stop_work(&root, &original.binding, Duration::from_secs(10))
                .await?;
        ensure!(
            stopped.phase == Phase::Ready && stopped.intent == Intent::Stopped,
            "repaired owner could not stop work"
        );
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "business survived stop"
        );
        let completed = original
            .generation
            .as_deref()
            .context("generation missing")?;
        runtime_supervisor::verify_quiescent(&root, completed)?;
        let commands = root.join("work").join(completed).join("commands");
        std::fs::create_dir_all(&commands)?;
        let history = commands.join("damaged-history.json");
        std::fs::write(&history, "{damaged-historical-command")?;

        // Finish all actual processes first. With discovery damaged again,
        // offline Stop must use generation evidence, not endpoint metadata.
        shutdown(&root, &mut parent).await;
        ensure!(parent.try_wait()?.is_some(), "supervisor did not exit");
        std::fs::write(root.join("supervisor.json"), broken)?;
        let done = runtime_supervisor::stop_work(&root, &original.binding, Duration::from_secs(10))
            .await?;
        ensure!(
            done.phase == Phase::Stopped && done.intent == Intent::Stopped,
            "offline Stop depended on damaged discovery"
        );
        ensure!(
            std::fs::read_to_string(&history)? == "{damaged-historical-command",
            "historical evidence was rewritten"
        );

        // Corruption on startup suppresses one-shot env replay and restores
        // management first. A later explicit request can restart normally.
        std::fs::write(root.join("supervisor.json"), broken)?;
        parent = tokio::process::Command::new(env!("CARGO_BIN_EXE_supervision-fixture"))
            .arg(&root)
            .env("SUPERVISION_FIXTURE_ONE_SHOT", "must-not-replay")
            .spawn()?;
        let next = until(&root, |s| s.phase == Phase::Ready).await?;
        ensure!(
            next.supervisor_id != original.supervisor_id,
            "startup reused old owner"
        );
        ensure!(
            std::fs::read_to_string(root.join("one-shot-env-present"))? == "false",
            "damaged discovery replayed deployment input"
        );
        ensure!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "management recovery started business implicitly"
        );
        control(&root, Request::new(Action::Recover)).await?;
        until(&root, |s| {
            s.phase == Phase::Ready && s.generation != next.generation
        })
        .await?;
        let restarted = leaf(&root).await?;
        runtime_supervisor::stop_work(&root, &original.binding, Duration::from_secs(10)).await?;
        ensure!(
            tokio::net::TcpStream::connect(&restarted).await.is_err(),
            "new execution could not be stopped"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(&root, &mut parent).await;
    result.unwrap();
}

#[tokio::test]
async fn failed_cleanup_attempt_can_be_retried_without_losing_original_failure() {
    use runtime_supervisor::{Binding, Intent, Phase};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    // Fixture for a cleanup result that arrives late. No real process is
    // represented by this record; only the admission/retry contract is tested.
    let id = uuid::Uuid::new_v4().to_string();
    let work = root.join("work").join(&id);
    std::fs::create_dir_all(&work).unwrap();
    let mut receipt = serde_json::json!({
        "version": 1, "id": id, "supervisor": "fixture-old", "token": "fixture",
        "intent": "run", "phase": "Running", "exit_code": null, "error": null
    });
    std::fs::write(
        work.join("generation.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let binding = Binding {
        component: "runtime".into(),
        resource: root.clone(),
    };
    let old = Snapshot {
        version: 1,
        binding: binding.clone(),
        supervisor_id: "fixture-old".into(),
        generation: Some(id.clone()),
        phase: Phase::RecoveryRequired,
        intent: Intent::Stopped,
        operation_id: None,
        error: None,
        problem: None,
    };
    std::fs::write(
        root.join("supervisor.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 2, "instance": "fixture-old", "address": "127.0.0.1:1",
            "token": "fixture", "snapshot": old, "requests": []
        }))
        .unwrap(),
    )
    .unwrap();
    let mut parent = start(&root).unwrap();
    let result = async {
        until(&root, |s| s.phase == Phase::RecoveryRequired).await?;
        let mut attempts = Vec::new();
        for _ in 0..2 {
            let error = runtime_supervisor::stop_work(&root, &binding, Duration::from_secs(5))
                .await
                .unwrap_err();
            let failure = &error
                .downcast_ref::<runtime_supervisor::RecoveryError>()
                .with_context(|| {
                    format!("cleanup must report its real failure, not permanent Busy: {error:#}")
                })?
                .snapshot;
            ensure!(
                failure.phase == Phase::RecoveryRequired,
                "cleanup falsely succeeded"
            );
            attempts.push(Request {
                request_id: failure
                    .operation_id
                    .clone()
                    .context("failure identity missing")?,
                action: Action::StopWork,
                expected_generation: Some(id.clone()),
            });
        }
        let first = attempts[0].clone();
        let retry = attempts[1].clone();
        ensure!(
            first.request_id != retry.request_id,
            "a new user retry replayed the old failed request"
        );
        ensure!(
            control(&root, first.clone()).await?.phase == Phase::RecoveryRequired,
            "new request changed prior failure to success"
        );
        receipt["phase"] = "Quiescent".into();
        // Use an atomic write like the real guardian so partial test writes
        // cannot manufacture a generation corruption unrelated to this case.
        let file = tempfile::NamedTempFile::new_in(&work)?;
        std::fs::write(file.path(), serde_json::to_vec(&receipt)?)?;
        file.persist(work.join("generation.json"))?;
        until(&root, |s| s.phase == Phase::Ready).await?;
        ensure!(
            control(&root, retry).await?.phase == Phase::Ready,
            "retry never completed"
        );
        ensure!(
            control(&root, first).await?.phase == Phase::RecoveryRequired,
            "old failure lost after recovery"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    shutdown(&root, &mut parent).await;
    result.unwrap();
}

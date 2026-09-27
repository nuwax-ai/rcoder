#![cfg(feature = "test-support")]
use anyhow::{Context, Result, ensure};
use runtime_supervisor::{Action, Request, Snapshot, control};
use std::{path::Path, time::Duration};

async fn status(root: &Path) -> Result<Snapshot> {
    control(root, Request::new(Action::Status)).await
}
async fn until(root: &Path, predicate: impl Fn(&Snapshot) -> bool) -> Result<Snapshot> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(value) = status(root).await
                && predicate(&value)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("supervisor did not reach expected state")
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
        .env("SUPERVISION_FIXTURE_ONE_SHOT", "old-deployment")
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
            std::fs::read_to_string(root.join("one-shot-env-present"))? == "false",
            "replacement replayed one-shot deployment input"
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

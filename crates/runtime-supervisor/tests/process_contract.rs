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
async fn hung_control_is_stoppable_retries_replay_and_business_stays_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("工作区 with spaces");
    std::fs::create_dir(&root).unwrap();
    let mut owner = start(&root).unwrap();
    let result = async {
        let old = until(&root, |s| s.phase == "ready").await?;
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
        ensure!(
            control(&root, Request::new(Action::Recover)).await.is_err(),
            "conflicting request was queued"
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
        until(&root, |s| s.phase == "ready").await?;
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
            control(&root, request).await?.phase == "ready",
            "completed retry lost terminal result"
        );
        let stopped = status(&root).await?;
        let recover = control(&root, Request::new(Action::Recover)).await?;
        ensure!(recover.phase == "stopping", "recover not admitted");
        until(&root, |s| {
            s.phase == "ready" && s.generation != stopped.generation
        })
        .await?;
        leaf(&root).await?;
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
        let original = until(&root, |s| s.phase == "ready").await?;
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
        let next = until(&root, |s| s.phase == "ready").await?;
        ensure!(
            next.supervisor_id != original.supervisor_id && next.generation != original.generation,
            "successor reused identity"
        );
        ensure!(
            control(&root, stop).await?.phase == "ready",
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

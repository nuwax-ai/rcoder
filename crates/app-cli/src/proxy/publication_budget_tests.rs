//! Real owned process trees and timer deadlines, with controlled HTTP outcomes.
//! These exercise the transaction budget; official Pingap acceptance is separate.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use process_utils::managed_tree::{ManagedChild, StopOutcome, spawn_managed};
use tokio::time::Instant;

use super::{ProcessHost, ProcessInstance, restart_verified};
use crate::proxy::{apply_status::ConfirmedPublication, compiler};

#[derive(Default)]
struct OwnedHost {
    child: Option<ManagedChild>,
    instance: Option<ProcessInstance>,
    exhaust_confirmation: bool,
    block_cleanup: bool,
    spawned: Vec<u32>,
    cleaned: Vec<u32>,
    committed: Option<String>,
}

#[async_trait::async_trait]
impl ProcessHost for OwnedHost {
    async fn stop_exact(&mut self, deadline: Instant) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        ensure!(
            !self.block_cleanup,
            "retained process cleanup cannot be confirmed"
        );
        // Deliberately needs non-zero settlement time after HTTP rejection.
        tokio::time::timeout_at(deadline, async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            ensure!(
                child.stop(Duration::ZERO).await != StopOutcome::Unconfirmed,
                "owned tree did not stop"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("owned tree cleanup deadline exhausted")??;
        self.cleaned
            .push(child.id().context("retained process PID missing")?);
        self.child.take();
        self.instance = None;
        Ok(())
    }
    async fn start_exact(
        &mut self,
        _candidate: &compiler::CompileOutcome,
        deadline: Instant,
    ) -> Result<ProcessInstance> {
        ensure!(
            Instant::now() < deadline && self.child.is_none(),
            "cannot replace retained tree"
        );
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30");
        let child = spawn_managed(command)?;
        let instance = ProcessInstance {
            pid: child.id().context("actual process PID missing")?,
            id: uuid::Uuid::new_v4().to_string(),
        };
        self.spawned.push(instance.pid);
        self.child = Some(child);
        self.instance = Some(instance.clone());
        Ok(instance)
    }
    async fn verify_exact(&mut self, expected: &ProcessInstance, deadline: Instant) -> Result<()> {
        ensure!(
            Instant::now() < deadline && self.instance.as_ref() == Some(expected),
            "process identity changed"
        );
        let child = self
            .child
            .as_mut()
            .context("retained actual tree missing")?;
        ensure!(
            child.id() == Some(expected.pid) && child.try_wait_root()?.is_none(),
            "actual tree exited"
        );
        Ok(())
    }
    async fn confirm(
        &mut self,
        candidate: &compiler::CompileOutcome,
        instance: &ProcessInstance,
        deadline: Instant,
    ) -> Result<ConfirmedPublication> {
        if self.exhaust_confirmation {
            tokio::time::sleep_until(deadline).await;
            anyhow::bail!("controlled HTTP observation exhausted its phase deadline");
        }
        Ok(ConfirmedPublication {
            process_id: instance.pid,
            instance_id: instance.id.clone(),
            publication_id: candidate.publication_id.clone(),
            operation_id: candidate.operation_id.clone(),
            config_hash: candidate.expected_hash.clone(),
            config_digest: candidate.config_digest.clone(),
            attempt_id: 1,
        })
    }
    async fn commit(
        &mut self,
        candidate: &compiler::CompileOutcome,
        _receipt: &ConfirmedPublication,
        _deadline: Instant,
    ) -> Result<()> {
        self.committed = Some(candidate.publication_id.clone());
        Ok(())
    }
    fn mark_unknown(&self, message: String) -> anyhow::Error {
        crate::supervisor::ShutdownUnconfirmed(message).into()
    }
}

async fn candidate(root: &std::path::Path) -> compiler::CompileOutcome {
    let id = uuid::Uuid::new_v4().to_string();
    let (text, _) = compiler::build_standby_config(&id).unwrap();
    let graph = pingap_config::PingapConfig::new(text.as_bytes(), true).unwrap();
    compiler::fixture_persist_candidate(root, &graph, &id, 503)
        .await
        .unwrap()
}

// Test teardown settles only the exact fixture handles. It never changes the
// captured original operation outcome or counts late cleanup as timely success.
async fn tear_down(host: &mut OwnedHost) {
    host.block_cleanup = false;
    host.stop_exact(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn cold_http_deadline_retains_real_cleanup_time_and_allows_fresh_retry() {
    let root = tempfile::tempdir().unwrap();
    let rejected = candidate(root.path()).await;
    let mut host = OwnedHost {
        exhaust_confirmation: true,
        ..Default::default()
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let error = restart_verified(&mut host, &rejected, None, deadline)
        .await
        .unwrap_err();
    let was_clean = host.child.is_none();
    let completed = Instant::now();
    let cleaned = host.cleaned.clone();
    let spawned = host.spawned.clone();
    tear_down(&mut host).await;
    assert!(
        !error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "{error:#}"
    );
    assert!(
        was_clean && completed < deadline,
        "cleanup was not confirmed within original deadline"
    );
    assert_eq!(spawned.len(), 1);
    assert_eq!(cleaned, spawned);
    assert!(!process_utils::process_group_exists(spawned[0]).unwrap());
    assert!(host.committed.is_none());
    host.exhaust_confirmation = false;
    let corrected = candidate(root.path()).await;
    let receipt = restart_verified(
        &mut host,
        &corrected,
        None,
        Instant::now() + Duration::from_secs(2),
    )
    .await;
    let confirmed_id = host.committed.clone();
    tear_down(&mut host).await;
    assert_eq!(receipt.unwrap().publication_id, corrected.publication_id);
    assert_eq!(
        confirmed_id.as_deref(),
        Some(corrected.publication_id.as_str())
    );
}

#[tokio::test]
async fn failed_restore_also_reserves_real_cleanup_within_original_deadline() {
    let root = tempfile::tempdir().unwrap();
    let original = candidate(root.path()).await;
    let rejected = candidate(root.path()).await;
    let mut host = OwnedHost {
        exhaust_confirmation: true,
        ..Default::default()
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let error = restart_verified(&mut host, &rejected, Some(&original), deadline)
        .await
        .unwrap_err();
    let was_clean = host.child.is_none();
    let completed = Instant::now();
    let cleaned = host.cleaned.clone();
    let spawned = host.spawned.clone();
    tear_down(&mut host).await;
    assert!(
        !error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "{error:#}"
    );
    assert!(was_clean && completed < deadline);
    assert_eq!(
        spawned.len(),
        2,
        "both candidate and restoration must actually launch"
    );
    assert_eq!(cleaned, spawned);
    for pid in spawned {
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }
    assert!(host.committed.is_none());
}

#[tokio::test]
async fn physically_unconfirmed_candidate_keeps_exact_tree_and_blocks_another_start() {
    let root = tempfile::tempdir().unwrap();
    let rejected = candidate(root.path()).await;
    let mut host = OwnedHost {
        exhaust_confirmation: true,
        block_cleanup: true,
        ..Default::default()
    };
    let error = restart_verified(
        &mut host,
        &rejected,
        None,
        Instant::now() + Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    let first_pid = host.child.as_ref().and_then(ManagedChild::id).unwrap();
    let second = restart_verified(
        &mut host,
        &rejected,
        None,
        Instant::now() + Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    let retained_pid = host.child.as_ref().and_then(ManagedChild::id);
    let live = process_utils::process_exists(first_pid).unwrap();
    let count = host.spawned.len();
    tear_down(&mut host).await;
    assert!(error.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(second.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(live);
    assert_eq!(retained_pid, Some(first_pid));
    assert_eq!(count, 1, "unknown cleanup authorized a second process");
}

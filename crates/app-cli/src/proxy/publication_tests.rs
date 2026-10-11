//! Controlled process-host fixtures exercise transition order and recovery.
//! They do not claim acceptance of a real Pingap binary or deployed container.
use super::{
    apply_status::ConfirmedPublication,
    compiler,
    publication::{self, ProcessHost, ProcessInstance},
};
use anyhow::{Context, Result};
use pingap_config::PingapConfig;
use std::path::Path;
use tokio::time::Instant;

#[derive(Default)]
struct Fixture {
    current: Option<ProcessInstance>,
    rejected: Option<String>,
    reject_all: bool,
    unknown_stop: bool,
    foreign_observation: bool,
    operations: Vec<String>,
    committed: Option<compiler::CompileOutcome>,
}

#[async_trait::async_trait]
impl ProcessHost for Fixture {
    async fn stop_exact(&mut self, _deadline: Instant) -> Result<()> {
        self.operations.push("stop".into());
        anyhow::ensure!(!self.unknown_stop, "owned process cleanup is unknown");
        self.current = None;
        Ok(())
    }
    async fn start_exact(
        &mut self,
        candidate: &compiler::CompileOutcome,
        _deadline: Instant,
    ) -> Result<ProcessInstance> {
        anyhow::ensure!(
            self.current.is_none(),
            "a previous owned process is still alive"
        );
        self.operations
            .push(format!("start:{}", candidate.publication_id));
        let instance = ProcessInstance {
            pid: 42,
            id: uuid::Uuid::new_v4().to_string(),
        };
        self.current = Some(instance.clone());
        Ok(instance)
    }
    async fn verify_exact(&mut self, expected: &ProcessInstance, _deadline: Instant) -> Result<()> {
        anyhow::ensure!(
            self.current.as_ref() == Some(expected),
            "owned process changed"
        );
        Ok(())
    }
    async fn confirm(
        &mut self,
        candidate: &compiler::CompileOutcome,
        instance: &ProcessInstance,
        _deadline: Instant,
    ) -> Result<ConfirmedPublication> {
        self.operations
            .push(format!("confirm:{}", candidate.publication_id));
        anyhow::ensure!(
            !self.reject_all && self.rejected.as_deref() != Some(&candidate.publication_id),
            "controlled candidate HTTP failure"
        );
        Ok(ConfirmedPublication {
            process_id: instance.pid,
            instance_id: if self.foreign_observation {
                uuid::Uuid::new_v4().to_string()
            } else {
                instance.id.clone()
            },
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
        self.operations
            .push(format!("commit:{}", candidate.publication_id));
        self.committed = Some(candidate.clone());
        Ok(())
    }
    fn mark_unknown(&self, message: String) -> anyhow::Error {
        crate::supervisor::ShutdownUnconfirmed(message).into()
    }
}

async fn candidate(root: &Path) -> compiler::CompileOutcome {
    let mut graph = PingapConfig::default();
    graph.servers.insert(
        "fixture".into(),
        pingap_config::ServerConf {
            addr: "127.0.0.1:9080".into(),
            ..Default::default()
        },
    );
    let id = uuid::Uuid::new_v4().to_string();
    let graph = compiler::publication_config(graph, &id, 200).unwrap();
    compiler::persist_candidate(root, &graph, &id, 200)
        .await
        .unwrap()
}
fn deadline() -> Instant {
    Instant::now() + std::time::Duration::from_secs(20)
}

#[tokio::test]
async fn rejected_candidate_restores_verified_snapshot_but_original_stays_failed() {
    let root = tempfile::tempdir().unwrap();
    let mut original = candidate(root.path()).await;
    original.business_probes.push(compiler::ServiceProbeTarget {
        service_id: "backend".into(),
        address: "127.0.0.1:1234".parse().unwrap(),
        path: "/health".into(),
    });
    let rejected = candidate(root.path()).await;
    let mut host = Fixture {
        rejected: Some(rejected.publication_id.clone()),
        ..Default::default()
    };
    let error = publication::restart_verified(&mut host, &rejected, Some(&original), deadline())
        .await
        .unwrap_err();
    assert!(
        !error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("previous verified configuration restored"));
    let restored = host.committed.as_ref().unwrap();
    assert_eq!(restored.publication_id, original.publication_id);
    assert_eq!(restored.business_probes.len(), 1);
    assert_eq!(
        host.operations,
        vec![
            "stop".into(),
            format!("start:{}", rejected.publication_id),
            format!("confirm:{}", rejected.publication_id),
            "stop".into(),
            format!("start:{}", original.publication_id),
            format!("confirm:{}", original.publication_id),
            format!("commit:{}", original.publication_id)
        ]
    );
}

#[tokio::test]
async fn rejected_bootstrap_without_snapshot_is_retryable_after_exact_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate(root.path()).await;
    let mut host = Fixture {
        reject_all: true,
        ..Default::default()
    };
    let error = publication::restart_verified(&mut host, &candidate, None, deadline())
        .await
        .unwrap_err();
    assert!(!error.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(host.current.is_none());
    assert!(host.committed.is_none());
    host.reject_all = false;
    publication::restart_verified(&mut host, &candidate, None, deadline())
        .await
        .unwrap();
    assert!(host.current.is_some());
    assert!(host.committed.is_some());
}

#[tokio::test]
async fn rejected_restoration_with_confirmed_cleanup_does_not_permanently_fence_source() {
    let root = tempfile::tempdir().unwrap();
    let original = candidate(root.path()).await;
    let candidate = candidate(root.path()).await;
    let mut host = Fixture {
        reject_all: true,
        ..Default::default()
    };
    let error = publication::restart_verified(&mut host, &candidate, Some(&original), deadline())
        .await
        .unwrap_err();
    assert!(
        !error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("cleanup confirmed"));
    assert!(host.current.is_none());
}

#[tokio::test]
async fn unconfirmed_stop_never_starts_or_restores_another_process() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate(root.path()).await;
    let mut host = Fixture {
        unknown_stop: true,
        ..Default::default()
    };
    let error = publication::restart_verified(&mut host, &candidate, None, deadline())
        .await
        .unwrap_err();
    assert!(error.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert_eq!(host.operations, ["stop"]);
}

#[tokio::test]
async fn foreign_observation_cannot_publish_success() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate(root.path()).await;
    let mut host = Fixture {
        foreign_observation: true,
        ..Default::default()
    };
    let error = publication::restart_verified(&mut host, &candidate, None, deadline())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("retained host instance"));
    assert!(host.committed.is_none());
    assert!(host.current.is_none());
}

#[tokio::test]
async fn modified_snapshot_is_rejected_before_old_process_stop() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate(root.path()).await;
    std::fs::write(&candidate.config_path, "[basic]\nname = 'changed'\n").unwrap();
    let mut host = Fixture::default();
    assert!(
        publication::restart_verified(&mut host, &candidate, None, deadline())
            .await
            .is_err()
    );
    assert!(host.operations.is_empty());
}

#[test]
fn digest_matches_upstream_canonical_fixture() {
    let config = PingapConfig::new(
        include_bytes!("../../tests/fixtures/applied-config.toml"),
        true,
    )
    .unwrap();
    assert_eq!(
        compiler::configuration_digest(&config)
            .context("canonical config digest")
            .unwrap(),
        include_str!("../../tests/fixtures/applied-config.sha256").trim()
    );
}

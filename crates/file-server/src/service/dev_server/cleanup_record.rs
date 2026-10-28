//! In-memory cleanup observations bound to the original local launch/child.
//! Native business generation and Stop request authority remain in the existing
//! StopWorkAttempt. This record cannot authorize a business stop or new start.
use super::supervise::SupervisedChild;
use super::types::CleanupStatus;
use crate::models::DevProcess;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct CleanupRecord {
    pub status: CleanupStatus,
    launch_id: Option<String>,
    pid: Option<u32>,
    started_at: Option<i64>,
    instance_id: Option<String>,
    child: Option<Arc<SupervisedChild>>,
}

impl CleanupRecord {
    pub fn capture(
        status: CleanupStatus,
        launch_id: Option<&str>,
        process: Option<&DevProcess>,
        child: Option<&Arc<SupervisedChild>>,
    ) -> Self {
        Self {
            status,
            launch_id: launch_id.map(str::to_owned),
            pid: process
                .map(|value| value.pid)
                .or_else(|| child.map(|value| value.pid())),
            started_at: process.map(|value| value.started_at),
            instance_id: process.and_then(|value| value.instance_id.clone()),
            child: child.cloned(),
        }
    }

    pub fn same_incarnation(&self, other: &Self) -> bool {
        self.launch_id == other.launch_id
            && self.pid == other.pid
            && self.started_at == other.started_at
            && self.instance_id == other.instance_id
            && match (&self.child, &other.child) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            }
    }

    pub fn completed(mut self) -> Self {
        self.status = CleanupStatus::Cleaned;
        self
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    async fn exited_child() -> Arc<SupervisedChild> {
        let child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child = SupervisedChild::adopt(child, Arc::new(Default::default()));
        assert!(matches!(child.wait_exit(Duration::from_secs(2)).await,
                         Some(super::super::supervise::ChildExit::Exited(status)) if status.success()));
        child
    }

    #[test]
    fn historical_cleanup_is_retained_but_does_not_gate_a_replacement_launch() {
        use super::super::support::lock;
        use super::super::types::{DevServerManager, LocalLaunch};
        let logs = tempfile::tempdir().unwrap();
        let config = crate::Config {
            log_base_dir: logs.path().to_path_buf(),
            ..Default::default()
        };
        let manager = DevServerManager::new(Arc::new(config));
        let key = "userapp:history-is-not-admission";
        let process = DevProcess {
            pid: 2_147_483_646,
            port: 9080,
            project_id: key.into(),
            started_at: 1,
            instance_id: None,
            base_path: None,
            log_dir: logs.path().to_path_buf(),
            temp_log_name: String::new(),
            external_owner: None,
        };
        lock(&manager.launches).unwrap().insert(
            key.into(),
            LocalLaunch {
                launch_id: "original-launch".into(),
                state_root: None,
            },
        );
        lock(&manager.processes)
            .unwrap()
            .insert(key.into(), process.clone());
        lock(&manager.cleanup_state).unwrap().insert(
            key.into(),
            CleanupRecord::capture(
                CleanupStatus::Cleaning,
                Some("original-launch"),
                Some(&process),
                None,
            ),
        );
        assert!(manager.has_uncleaned_cleanup(key).unwrap());
        lock(&manager.launches)
            .unwrap()
            .get_mut(key)
            .unwrap()
            .launch_id = "replacement-launch".into();
        assert!(!manager.has_uncleaned_cleanup(key).unwrap());
        let cleanup = lock(&manager.cleanup_state).unwrap();
        let retained = cleanup.get(key).unwrap();
        assert!(matches!(retained.status, CleanupStatus::Cleaning));
        assert_eq!(retained.launch_id.as_deref(), Some("original-launch"));
        drop(cleanup);
        // No fixture PID was spawned. Do not let Drop's best-effort shutdown
        // mistake this synthetic registration for signal authority.
        lock(&manager.processes).unwrap().remove(key);
    }

    #[tokio::test]
    async fn identical_numeric_pid_does_not_make_a_replacement_child_the_original_generation() {
        let first = exited_child().await;
        let second = exited_child().await;
        let process = DevProcess {
            pid: 42,
            port: 9080,
            project_id: "project".into(),
            started_at: 1,
            instance_id: None,
            base_path: None,
            log_dir: "/unused".into(),
            temp_log_name: String::new(),
            external_owner: None,
        };
        let original = CleanupRecord::capture(
            CleanupStatus::Cleaning,
            Some("original-launch"),
            Some(&process),
            Some(&first),
        );
        let same = CleanupRecord::capture(
            CleanupStatus::Cleaning,
            Some("original-launch"),
            Some(&process),
            Some(&first),
        );
        let replaced_child = CleanupRecord::capture(
            CleanupStatus::Cleaning,
            Some("original-launch"),
            Some(&process),
            Some(&second),
        );
        let replaced_launch = CleanupRecord::capture(
            CleanupStatus::Cleaning,
            Some("replacement-launch"),
            Some(&process),
            Some(&first),
        );
        assert!(original.same_incarnation(&same));
        assert!(!original.same_incarnation(&replaced_child));
        assert!(!original.same_incarnation(&replaced_launch));
    }
}

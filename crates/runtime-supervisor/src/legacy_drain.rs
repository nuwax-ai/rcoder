//! Drain a captured legacy root while retaining its original owner lease.
//! No replacement supervisor is started and no migration command is replayed.
use crate::{Binding, CleanupCommand, Owner, record};
use anyhow::{Context, Result};

impl Owner {
    pub async fn drain_previous_ranges(
        &self,
        binding: &Binding,
        application_id: &str,
        cleanup: &CleanupCommand,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let (discovery, _) = self.load_discovery(binding)?;
        let owner = discovery
            .as_ref()
            .map(|discovery| discovery.instance.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        loop {
            let result = tokio::time::timeout_at(deadline, async {
                while let Some(target) = record::reconcile(self.scope_root())? {
                    crate::cleanup::abandoned(target, Some(cleanup)).await?;
                }
                let identity = process_utils::command_authority::ResidentIdentity {
                    application_id: application_id.to_owned(),
                    binding: serde_json::to_value(binding)?,
                    owner_instance: owner.clone(),
                    physical_domain: crate::domain::PhysicalDomain::from_env()?
                        .map(serde_json::to_value)
                        .transpose()?,
                    process_epoch: crate::epoch::current(),
                };
                crate::resident::reconcile_prior_until(self.scope_root(), &identity, None, deadline)
                    .await
            })
            .await
            .context("legacy execution cleanup deadline exceeded")?;
            match result {
                Ok(()) => return Ok(()),
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::warn!(root = %self.scope_root().display(), %error, "legacy execution cleanup is still pending");
                }
                Err(error) => {
                    return Err(error).context("legacy execution did not reach actual quiescence");
                }
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + std::time::Duration::from_millis(100),
            ))
            .await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::BufRead;

    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _result = self.0.kill();
            let _result = self.0.wait();
        }
    }

    #[tokio::test]
    async fn dead_legacy_owner_with_a_real_live_command_waits_then_drains_without_new_owner() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let binding = Binding {
            component: "app-cli".into(),
            resource: root.clone(),
        };
        let supervisor = uuid::Uuid::new_v4().to_string();
        record::save(&root.join("supervisor.json"), &serde_json::json!({
            "version":2,"instance":supervisor,"address":"127.0.0.1:1","token":"fixture", "requests":[],
            "snapshot":{"version":1,"binding":binding,"supervisor_id":supervisor,"generation":null,
                "phase":"stopped","intent":"stopped","operation_id":null,"error":null,"problem":null}
        })).unwrap();
        let script = "import fcntl,sys,time; f=open(sys.argv[1], 'a+'); fcntl.flock(f,fcntl.LOCK_EX); print('locked',flush=True); time.sleep(60)";
        let mut old_owner = Process(
            std::process::Command::new("python3")
                .args(["-c", script])
                .arg(root.join("owner.lock"))
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut output = std::io::BufReader::new(old_owner.0.stdout.take().unwrap());
        let mut ready = String::new();
        output.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "locked");
        assert!(Owner::try_acquire(&root).unwrap().is_none());
        let mut command = Process(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let resident = root.join("resident").join(&supervisor);
        let guardian_id = uuid::Uuid::new_v4().to_string();
        let guardian = resident.join("guardians").join(&guardian_id);
        std::fs::create_dir_all(&guardian).unwrap();
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "app".into(),
            binding: serde_json::to_value(&binding).unwrap(),
            owner_instance: supervisor.clone(),
            physical_domain: None,
            process_epoch: crate::epoch::current(),
        };
        record::save(
            &resident.join("resident-scope.json"),
            &serde_json::json!({
                "version":1,"identity":identity,"phase":"Running","error":null
            }),
        )
        .unwrap();
        record::save(&guardian.join("receipt.json"), &serde_json::json!({
            "version":2,"id":guardian_id,"instance_id":supervisor,"phase":"Running", "command_record":null,
            "command_digest":"0".repeat(64),"root_status":null,"diagnostic_pid":command.0.id(),"resident_identity":identity
        })).unwrap();
        let before = std::fs::read(guardian.join("receipt.json")).unwrap();
        old_owner.0.kill().unwrap();
        old_owner.0.wait().unwrap();
        let successor = Owner::try_acquire(&root).unwrap().unwrap();
        let cleanup = CleanupCommand {
            program: "/bin/false".into(),
            args: vec![],
            cwd: root.clone(),
        };
        let recovery = successor.drain_previous_ranges(
            &binding,
            "app",
            &cleanup,
            tokio::time::Instant::now() + std::time::Duration::from_secs(3),
        );
        tokio::pin!(recovery);
        tokio::select! {
            result = &mut recovery => panic!("live original command must prevent quiescence: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        assert_eq!(
            std::fs::read(guardian.join("receipt.json")).unwrap(),
            before
        );
        assert!(
            Owner::try_acquire(&root).unwrap().is_none(),
            "one captured owner retains the real legacy lock"
        );
        command.0.kill().unwrap();
        command.0.wait().unwrap();
        recovery.await.unwrap();
        let settled: serde_json::Value =
            serde_json::from_slice(&std::fs::read(guardian.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(settled["phase"], "Quiescent");
        assert!(settled["root_status"].is_null());
        assert!(root.join("owner.lock").exists());
    }
}

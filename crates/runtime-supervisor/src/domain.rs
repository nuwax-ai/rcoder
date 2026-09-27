//! Physical-runtime exit evidence. This is only local process cleanup evidence;
//! deployment, migration and external-operation journals are never completed here.
use crate::{
    Binding,
    record::{self, GenerationPhase},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DOMAIN_ENV: &str = "RCODER_EXECUTION_DOMAIN";
pub const DOMAIN_LABEL: &str = "rcoder.io/execution-domain";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalDomain {
    pub authority: String,
    /// When the platform cannot know the execution instance at stamp time
    /// (a K8s pod UID only exists after scheduling), the stamp references the
    /// env var that carries it and `instance` is left empty; resolution to a
    /// concrete value happens in [`PhysicalDomain::from_env`]. The field is
    /// preserved on purpose so domain equality stays meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_source_env: Option<String>,
    pub instance: String,
    /// Stable mounted data sources, supplied by the runtime, not a Pod name.
    pub volume: String,
}
impl PhysicalDomain {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(value) = std::env::var_os(DOMAIN_ENV) else {
            return Ok(None);
        };
        let mut value: Self = serde_json::from_str(
            value
                .to_str()
                .context("invalid execution domain encoding")?,
        )?;
        if let Some(source) = value.instance_source_env.clone()
            && value.instance.is_empty()
        {
            value.instance = std::env::var(&source)
                .with_context(|| format!("execution domain instance source {source} missing"))?;
        }
        ensure!(
            !value.authority.is_empty() && !value.volume.is_empty() && !value.instance.is_empty(),
            "invalid execution domain"
        );
        Ok(Some(value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retirement {
    pub binding: Binding,
    pub generation: String,
    pub supervisor_id: String,
    pub domain: PhysicalDomain,
}

/// Read-only proposals. A proposal is never itself authority to retire work.
pub fn pending(root: &Path, binding: &Binding) -> Result<Vec<Retirement>> {
    if !root.join("supervisor.json").try_exists()? {
        return Ok(Vec::new());
    }
    ensure!(
        &crate::last_snapshot(root)?.binding == binding,
        "supervisor binding differs"
    );
    let mut result = Vec::new();
    let work = root.join("work");
    if !work.try_exists()? {
        return Ok(result);
    }
    for entry in std::fs::read_dir(work)? {
        let path = entry?.path();
        if !path.join("generation.json").try_exists()? {
            continue;
        }
        let value = record::generation(&path)?;
        if matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        ) && let Some(domain) = value.physical_domain
        {
            result.push(Retirement {
                binding: binding.clone(),
                generation: value.id,
                supervisor_id: value.supervisor,
                domain,
            });
        }
    }
    Ok(result)
}

/// Called only after the platform has positively verified that the original
/// physical domain cannot run again. In particular, missing liveness is NOT proof.
/// The receipt is consumed under the original generation/guardian locks.
pub fn publish_confirmed_exit(root: &Path, expected: &Binding, proof: &Retirement) -> Result<()> {
    let current = PhysicalDomain::from_env()?.context("successor physical domain missing")?;
    publish_with_current(root, expected, proof, &current)
}
fn publish_with_current(
    root: &Path,
    expected: &Binding,
    proof: &Retirement,
    current: &PhysicalDomain,
) -> Result<()> {
    ensure!(
        &proof.binding == expected && crate::last_snapshot(root)?.binding == *expected,
        "retirement belongs to another workspace"
    );
    ensure!(
        current.authority == proof.domain.authority
            && current.volume == proof.domain.volume
            && current.instance != proof.domain.instance,
        "retirement physical binding differs"
    );
    let work = record::work_root(root, &proof.generation)?;
    let value = record::generation(&work)?;
    ensure!(
        value.supervisor == proof.supervisor_id
            && value.physical_domain.as_ref() == Some(&proof.domain),
        "retirement generation identity differs"
    );
    record::save(&work.join("physical-exit.json"), proof)
}

pub(crate) fn has_confirmed_exit(root: &Path, value: &record::Generation) -> Result<bool> {
    let file = root.join("physical-exit.json");
    if !file.try_exists()? {
        return Ok(false);
    }
    let proof: Retirement = record::read(&file)?;
    let scope = root
        .parent()
        .and_then(Path::parent)
        .context("generation scope missing")?;
    ensure!(
        crate::last_snapshot(scope)?.binding == proof.binding
            && value.id == proof.generation
            && value.supervisor == proof.supervisor_id
            && value.physical_domain.as_ref() == Some(&proof.domain),
        "physical exit receipt identity differs"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_exit_is_identity_bound_and_never_completes_business_work() {
        let temp = tempfile::tempdir().unwrap();
        let scope = temp.path();
        let _owner = crate::Owner::try_acquire(scope).unwrap().unwrap();
        let binding = Binding {
            component: "app-cli".into(),
            resource: scope.to_path_buf(),
        };
        let domain = PhysicalDomain {
            authority: "daemon-a".into(),
            instance_source_env: None,
            volume: "volume-a".into(),
            instance: uuid::Uuid::new_v4().to_string(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let root = record::work_root(scope, &id).unwrap();
        std::fs::create_dir_all(root.join("commands")).unwrap();
        process_utils::command_authority::Gate::try_acquire(&root)
            .unwrap()
            .initialize()
            .unwrap();
        let generation = record::Generation {
            version: 1,
            id,
            supervisor: "old-supervisor".into(),
            token: "test".into(),
            intent: crate::Intent::Run,
            phase: GenerationPhase::Running,
            worker_pid: None,
            exit_code: None,
            error: None,
            physical_domain: Some(domain.clone()),
            process_epoch: None,
        };
        record::save(&root.join("generation.json"), &generation).unwrap();
        record::save(&scope.join("supervisor.json"), &serde_json::json!({
            "version":1, "instance":"old-supervisor", "address":"127.0.0.1:1", "token":"test", "requests":[],
            "snapshot":{ "version":1,"binding":binding,"supervisor_id":"old-supervisor", "generation":generation.id,
                "phase":"ready", "intent":"run", "operation_id":null,"error":null }
        })).unwrap();
        record::save(
            &root.join("commands/command.json"),
            &serde_json::json!({"version":1,"phase":"Running","identity":{"task_id":"original"}}),
        )
        .unwrap();
        std::fs::write(scope.join("migration.json"), "outcome unknown").unwrap();
        let proof = pending(scope, &binding).unwrap().remove(0);
        assert!(
            record::reconcile(scope).is_err(),
            "free locks alone cannot retire work"
        );
        let mut successor = domain.clone();
        assert!(publish_with_current(scope, &binding, &proof, &successor).is_err());
        successor.instance = uuid::Uuid::new_v4().to_string();
        successor.volume = "wrong-volume".into();
        assert!(publish_with_current(scope, &binding, &proof, &successor).is_err());
        successor.volume = domain.volume;
        let mut wrong = proof.clone();
        wrong.supervisor_id = "different-supervisor".into();
        assert!(publish_with_current(scope, &binding, &wrong, &successor).is_err());
        publish_with_current(scope, &binding, &proof, &successor).unwrap();
        let held = record::lock(&root.join("generation.lock")).unwrap();
        assert!(
            record::reconcile(scope).is_err(),
            "live generation lock is never bypassed"
        );
        drop(held);
        record::reconcile(scope).unwrap();
        crate::verify_quiescent(scope, &generation.id).unwrap();
        let retired = record::generation(&root).unwrap();
        assert_eq!(retired.phase, GenerationPhase::Quiescent);
        assert_eq!(retired.exit_code, None);
        assert_eq!(
            std::fs::read_to_string(scope.join("migration.json")).unwrap(),
            "outcome unknown"
        );
        let command: serde_json::Value = record::read(&root.join("commands/command.json")).unwrap();
        assert_eq!(command["phase"], "Quiescent");
        assert_eq!(command["identity"]["task_id"], "original");
    }
}

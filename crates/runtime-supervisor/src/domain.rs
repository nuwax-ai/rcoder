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

/// 固定只读平台位置（recovery v2 plan §6.1）：容器当前绑定由运行时经
/// Downward API 卷投放到这里，管理进程直接读取——不经每条 spawn 链透传
/// env。目录可经本 env 覆盖（测试注入）；env 仍是第一优先来源（过渡桥）。
pub const PLATFORM_BINDING_DIR_ENV: &str = "RCODER_PLATFORM_BINDING_DIR";
pub const DEFAULT_PLATFORM_BINDING_DIR: &str = "/etc/rcoder/platform";

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
        if let Some(value) = std::env::var_os(DOMAIN_ENV) {
            let value: Self = Self::decode(
                value
                    .to_str()
                    .context("invalid execution domain encoding")?,
            )?;
            let value =
                Self::resolve_instance_source_with_dir(value, &Self::platform_binding_dir())?;
            value.validate()?;
            return Ok(Some(value));
        }
        Self::from_platform_location()
    }

    /// Read the current binding from the fixed read-only platform location.
    /// `Ok(None)` means the location carries no binding (native/host runs);
    /// a present-but-invalid file is an error, never silently ignored.
    pub fn from_platform_location() -> Result<Option<Self>> {
        let dir = Self::platform_binding_dir();
        let raw = match Self::decode_checked_from(&dir.join("execution-domain")) {
            None => return Ok(None),
            Some(result) => result?,
        };
        let value: Self = Self::resolve_instance_source_with_dir(raw, &dir)?;
        value.validate()?;
        Ok(Some(value))
    }

    fn decode(raw: &str) -> Result<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Read and decode a binding file; None when the file is absent.
    fn decode_checked_from(path: &Path) -> Option<Result<Self>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Some(Self::decode(raw.trim())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(Err(error).context("read platform execution-domain binding")),
        }
    }

    /// Resolve an empty `instance` through its declared source: the platform
    /// companion file under `dir` first (fixed location), then the env bridge.
    fn resolve_instance_source_with_dir(mut value: Self, dir: &Path) -> Result<Self> {
        if let Some(source) = value.instance_source_env.clone()
            && value.instance.is_empty()
        {
            let companion = std::fs::read_to_string(dir.join(&source))
                .ok()
                .map(|s| s.trim().to_owned())
                .or_else(|| std::env::var(&source).ok());
            value.instance = companion
                .with_context(|| format!("execution domain instance source {source} missing"))?;
        }
        Ok(value)
    }

    fn platform_binding_dir() -> std::path::PathBuf {
        std::env::var_os(PLATFORM_BINDING_DIR_ENV)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_PLATFORM_BINDING_DIR))
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.authority.is_empty() && !self.volume.is_empty() && !self.instance.is_empty(),
            "invalid execution domain"
        );
        Ok(())
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

    const BINDING: &str = r#"{"authority":"k8s:test","instance":"","instance_source_env":"RCODER_PHYSICAL_POD_UID","volume":"pvc:ws"}"#;

    /// T4（recovery v2 plan §6.1）：固定只读平台位置承载当前绑定——
    /// execution-domain 文件 + 同伴 pod-uid 文件（Downward API 投放）。
    /// 实例解析文件优先于 env；损坏的绑定文件是错误，不静默忽略。
    #[test]
    fn platform_location_resolves_binding_without_env_chain() {
        let dir = tempfile::tempdir().unwrap();
        // 目录缺失 → 无绑定（native/宿主机语义）。
        assert!(PhysicalDomain::decode_checked_from(&dir.path().join("absent")).is_none());

        std::fs::write(dir.path().join("execution-domain"), BINDING).unwrap();
        std::fs::write(dir.path().join("RCODER_PHYSICAL_POD_UID"), "  uid-123 \n").unwrap();
        let value = PhysicalDomain::decode_checked_from(&dir.path().join("execution-domain"))
            .expect("binding file exists")
            .expect("valid binding");
        let resolved = PhysicalDomain::resolve_instance_source_with_dir(value, dir.path()).unwrap();
        assert_eq!(resolved.instance, "uid-123");
        assert_eq!(resolved.authority, "k8s:test");
        resolved.validate().unwrap();

        // 损坏文件 → 错误（不是 None）。
        std::fs::write(dir.path().join("execution-domain"), "{damaged").unwrap();
        assert!(
            PhysicalDomain::decode_checked_from(&dir.path().join("execution-domain"))
                .expect("damaged file still present")
                .is_err()
        );
    }
}

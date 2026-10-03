//! Authority for repairing a platform-managed builder's workspace binding.
//! Standalone project aliases and arbitrary same-prefix paths grant no such authority.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedWorkspace {
    pub application_id: String,
    pub source_root: PathBuf,
    pub state_root: PathBuf,
}

impl ManagedWorkspace {
    pub fn from_env(workspace: &Path, state_root: &Path) -> Result<Option<Self>> {
        Self::from_values(workspace, state_root, |key| std::env::var_os(key))
    }

    /// Inject the platform environment without mutating process-global variables.
    pub fn from_values(
        workspace: &Path,
        state_root: &Path,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Option<Self>> {
        let service = lookup("SERVICE_TYPE");
        let managed = lookup("APP_CLI_MANAGED");
        let declared = lookup("APP_CLI_RUNTIME_WORKSPACE");
        // Production and standalone launches may also set APP_CLI_MANAGED.
        // Only the explicitly enabled builder service opens this repair policy.
        if service.as_deref() != Some(std::ffi::OsStr::new("userapp-builder"))
            || managed.as_deref() != Some(std::ffi::OsStr::new("1"))
        {
            return Ok(None);
        }
        let application_id = lookup("PROJECT_ID")
            .context("managed workspace PROJECT_ID missing")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("managed workspace PROJECT_ID is not UTF-8"))?;
        let mut parts = Path::new(&application_id).components();
        ensure!(
            !application_id.trim().is_empty()
                && matches!(parts.next(), Some(Component::Normal(_)))
                && parts.next().is_none(),
            "managed workspace PROJECT_ID must be one normal path component"
        );
        let source_root = canonical_with_missing_tail(Path::new(
            &declared.context("managed workspace APP_CLI_RUNTIME_WORKSPACE missing")?,
        ))?;
        ensure!(
            source_root.file_name() == Some(std::ffi::OsStr::new(&application_id)),
            "managed source root does not match PROJECT_ID"
        );
        let origin = super::resolve_project_origin(workspace)?;
        ensure!(
            canonical_with_missing_tail(&origin)? == source_root,
            "requested workspace is not the declared managed source root"
        );
        let explicit =
            lookup("APP_CLI_STATE_ROOT").context("managed workspace APP_CLI_STATE_ROOT missing")?;
        let state_root = canonical_with_missing_tail(state_root)?;
        ensure!(
            state_root.starts_with(&source_root)
                && canonical_with_missing_tail(Path::new(&explicit))? == state_root
                && canonical_with_missing_tail(&source_root.join("state").join(&application_id))?
                    == state_root,
            "managed state root is not the application's declared state authority"
        );
        Ok(Some(Self {
            application_id,
            source_root,
            state_root,
        }))
    }

    /// Deleted staging directories remain addressable through their nearest
    /// existing ancestor. Existing symlinks must still resolve inside this app.
    pub fn verify_contained_workspace(&self, candidate: &Path) -> Result<PathBuf> {
        let physical = canonical_with_missing_tail(candidate)?;
        ensure!(
            physical.starts_with(&self.source_root),
            "owner workspace is outside the managed application"
        );
        // A corrupt or copied provenance marker is not bypassed by containment.
        let origin = super::resolve_project_origin(candidate)?;
        ensure!(
            canonical_with_missing_tail(&origin)?.starts_with(&self.source_root),
            "owner project origin is outside the managed application"
        );
        Ok(physical)
    }
}

/// Resolve every existing path component before retaining a missing suffix.
/// Never fall back to the full lexical path after canonicalization fails: that
/// would grant containment through a symlink whose target escaped the project.
fn canonical_with_missing_tail(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "managed workspace paths must be absolute"
    );
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "managed workspace path must not contain parent traversal"
    );
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut physical = std::fs::canonicalize(ancestor)
                    .with_context(|| format!("resolve managed path {}", ancestor.display()))?;
                for segment in suffix.into_iter().rev() {
                    physical.push(segment);
                }
                return Ok(physical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .context("managed path has no existing ancestor")?,
                );
                ancestor = ancestor.parent().context("managed path has no parent")?;
            }
            Err(error) => return Err(error).context("inspect managed workspace path"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        Vec<(&'static str, OsString)>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let source = std::fs::canonicalize(temp.path()).unwrap().join("11");
        let state = source.join("state/11");
        std::fs::create_dir_all(&state).unwrap();
        let env = vec![
            ("SERVICE_TYPE", "userapp-builder".into()),
            ("APP_CLI_MANAGED", "1".into()),
            ("PROJECT_ID", "11".into()),
            ("APP_CLI_RUNTIME_WORKSPACE", source.clone().into_os_string()),
            ("APP_CLI_STATE_ROOT", state.clone().into_os_string()),
        ];
        (temp, source, state, env)
    }

    #[test]
    fn managed_authority_requires_complete_platform_identity_and_state_scope() {
        let (_temp, source, state, env) = fixture();
        let lookup = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        };
        let managed = ManagedWorkspace::from_values(&source, &state, lookup)
            .unwrap()
            .unwrap();
        assert_eq!(managed.application_id, "11");
        assert_eq!(managed.source_root, source);
        assert!(ManagedWorkspace::from_values(&source.join("code"), &state, lookup).is_err());
        assert!(ManagedWorkspace::from_values(&source, &source.join("state/12"), lookup).is_err());
        for missing in [
            "PROJECT_ID",
            "APP_CLI_RUNTIME_WORKSPACE",
            "APP_CLI_STATE_ROOT",
        ] {
            assert!(
                ManagedWorkspace::from_values(&source, &state, |key| if key == missing {
                    None
                } else {
                    lookup(key)
                })
                .is_err(),
                "{missing}"
            );
        }
        assert!(
            ManagedWorkspace::from_values(&source, &state, |_| None)
                .unwrap()
                .is_none()
        );
        for (service, enabled) in [
            (None, Some("1")),
            (Some("app-runtime"), Some("1")),
            (Some("userapp-builder"), None),
            (Some("userapp-builder"), Some("0")),
        ] {
            assert!(
                ManagedWorkspace::from_values(
                    &source.join("other"),
                    &source.join("custom-state"),
                    |key| match key {
                        "SERVICE_TYPE" => service.map(OsString::from),
                        "APP_CLI_MANAGED" => enabled.map(OsString::from),
                        _ => lookup(key),
                    }
                )
                .unwrap()
                .is_none(),
                "non-enabled builder contexts keep their existing layout"
            );
        }
    }

    #[test]
    fn missing_child_is_contained_but_sibling_and_parent_traversal_are_not() {
        let (_temp, source, state, env) = fixture();
        let managed = ManagedWorkspace::from_values(&source, &state, |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            managed
                .verify_contained_workspace(&source.join("gone/code"))
                .unwrap(),
            source.join("gone/code")
        );
        assert!(
            managed
                .verify_contained_workspace(&source.with_file_name("111"))
                .is_err()
        );
        assert!(
            managed
                .verify_contained_workspace(&source.join("gone/../../12"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_tail_through_symlink_cannot_escape_managed_root() {
        let (temp, source, state, env) = fixture();
        let managed = ManagedWorkspace::from_values(&source, &state, |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
        .unwrap();
        let foreign = temp.path().join("12");
        std::fs::create_dir_all(&foreign).unwrap();
        std::os::unix::fs::symlink(&foreign, source.join("foreign")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("missing-foreign"), source.join("broken"))
            .unwrap();
        assert!(
            managed
                .verify_contained_workspace(&source.join("foreign/deleted"))
                .is_err()
        );
        assert!(
            managed
                .verify_contained_workspace(&source.join("broken/deleted"))
                .is_err()
        );
        std::os::unix::fs::symlink(&source, source.join("inside")).unwrap();
        assert!(
            managed
                .verify_contained_workspace(&source.join("inside/deleted"))
                .is_ok()
        );
    }
}

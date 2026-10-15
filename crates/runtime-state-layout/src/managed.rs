//! Authority for repairing a platform-managed builder's workspace binding.
//! Standalone project aliases and arbitrary same-prefix paths grant no such authority.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use shared_types::ServiceType;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedWorkspace {
    pub application_id: String,
    pub source_root: PathBuf,
    pub state_root: PathBuf,
    /// The existing single-app file-server contract treats its workspace env
    /// as the complete source path rather than a parent directory.
    pub single_application: bool,
}

impl ManagedWorkspace {
    /// ServiceType owns the accepted wire spellings. Other service families
    /// never receive builder repair authority, even if APP_CLI_MANAGED is set.
    pub fn enabled_from_values(lookup: impl Fn(&str) -> Option<OsString>) -> bool {
        lookup("APP_CLI_MANAGED").as_deref() == Some(std::ffi::OsStr::new("1"))
            && lookup("SERVICE_TYPE")
                .and_then(|service| service.into_string().ok())
                .and_then(|service| service.parse::<ServiceType>().ok())
                == Some(ServiceType::UserappBuilder)
    }

    pub fn from_env(workspace: &Path, state_root: &Path) -> Result<Option<Self>> {
        Self::from_values(workspace, state_root, |key| std::env::var_os(key))
    }

    /// Inject the platform environment without mutating process-global variables.
    pub fn from_values(
        workspace: &Path,
        state_root: &Path,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Option<Self>> {
        // Production and standalone launches may also set APP_CLI_MANAGED.
        // Only the explicitly enabled builder service opens this repair policy.
        if !Self::enabled_from_values(&lookup) {
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
        let base = lookup("USERAPP_WORKSPACE_DIR")
            .context("managed workspace USERAPP_WORKSPACE_DIR missing")?;
        let base = canonical_with_missing_tail(Path::new(&base))?;
        let single = lookup("USERAPP_SINGLE_APP_ID")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("USERAPP_SINGLE_APP_ID is not UTF-8"))
            })
            .transpose()?;
        let single = single
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let source_root = if let Some(single) = single {
            ensure!(
                single == application_id,
                "managed single-app identity does not match PROJECT_ID"
            );
            ensure!(
                base.file_name() == Some(std::ffi::OsStr::new(&application_id)),
                "managed single-app source does not match PROJECT_ID"
            );
            base
        } else {
            let source_root = canonical_with_missing_tail(&base.join(&application_id))?;
            ensure!(
                source_root == base.join(&application_id),
                "managed application source resolves outside its platform path"
            );
            source_root
        };
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
        let context = Self {
            application_id,
            source_root,
            state_root,
            single_application: single.is_some(),
        };
        // The old source/code declaration is a proven platform layout, not an
        // authority that can override the file/build workspace contract.
        if let Some(declared) = lookup("APP_CLI_RUNTIME_WORKSPACE") {
            context.management_workspace(Path::new(&declared))?;
        }
        context.management_workspace(workspace)?;
        Ok(Some(context))
    }

    /// Normalize only the obsolete exact source/code management root. Artifact
    /// execution directories and explicit provenance keep their own location.
    pub fn management_workspace(&self, workspace: &Path) -> Result<PathBuf> {
        let workspace = if workspace.is_absolute() {
            workspace.to_path_buf()
        } else {
            std::env::current_dir()
                .context("resolve management workspace cwd")?
                .join(workspace)
        };
        let physical = self.verify_contained_workspace(&workspace)?;
        if physical == self.source_root {
            ensure!(
                canonical_with_missing_tail(&super::resolve_project_origin(&workspace)?)?
                    == self.source_root,
                "managed source root has a foreign project origin"
            );
            return Ok(self.source_root.clone());
        }
        // Compare the lexical path too: an escaping/surprising symlink must not
        // become eligible merely because its basename happens to be code.
        if workspace == self.source_root.join("code") && physical == self.source_root.join("code") {
            let origin = canonical_with_missing_tail(&super::resolve_project_origin(&workspace)?)?;
            ensure!(
                origin == physical || origin == self.source_root,
                "legacy managed code directory has a foreign project origin"
            );
            return Ok(self.source_root.clone());
        }
        ensure!(
            canonical_with_missing_tail(&super::resolve_project_origin(&workspace)?)?
                == self.source_root,
            "requested workspace is not the managed source or a proven execution alias"
        );
        Ok(physical)
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

    /// Complete platform authority for management children whose environment is
    /// otherwise cleared. Business credentials are intentionally absent here.
    pub fn launch_environment(&self) -> Result<Vec<(String, String)>> {
        let as_utf8 = |path: &Path| -> Result<String> {
            Ok(path
                .to_str()
                .context("managed launch path is not UTF-8")?
                .to_owned())
        };
        let base = if self.single_application {
            self.source_root.as_path()
        } else {
            self.source_root
                .parent()
                .context("managed source has no workspace base")?
        };
        let mut environment = vec![
            (
                "SERVICE_TYPE".into(),
                ServiceType::UserappBuilder.to_string(),
            ),
            ("APP_CLI_MANAGED".into(), "1".into()),
            ("PROJECT_ID".into(), self.application_id.clone()),
            ("USERAPP_WORKSPACE_DIR".into(), as_utf8(base)?),
            (
                "APP_CLI_RUNTIME_WORKSPACE".into(),
                as_utf8(&self.source_root)?,
            ),
            ("APP_CLI_STATE_ROOT".into(), as_utf8(&self.state_root)?),
        ];
        if self.single_application {
            environment.push(("USERAPP_SINGLE_APP_ID".into(), self.application_id.clone()));
        }
        Ok(environment)
    }
}

/// Shared by CLI and library management entrypoints before binding or locking.
/// Build and validate deliberately do not use this normalization.
pub fn normalize_management_workspace(workspace: &Path) -> Result<PathBuf> {
    normalize_management_workspace_from_values(workspace, |key| std::env::var_os(key))
}

pub fn normalize_management_workspace_from_values(
    workspace: &Path,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf> {
    if !ManagedWorkspace::enabled_from_values(&lookup) {
        return Ok(workspace.to_path_buf());
    }
    let state =
        lookup("APP_CLI_STATE_ROOT").context("managed workspace APP_CLI_STATE_ROOT missing")?;
    let managed = ManagedWorkspace::from_values(workspace, Path::new(&state), lookup)?
        .context("managed builder declaration changed during normalization")?;
    managed.management_workspace(workspace)
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
            ("SERVICE_TYPE", "user-app-builder".into()),
            ("APP_CLI_MANAGED", "1".into()),
            ("PROJECT_ID", "11".into()),
            (
                "USERAPP_WORKSPACE_DIR",
                source.parent().unwrap().as_os_str().to_owned(),
            ),
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
        assert!(ManagedWorkspace::from_values(&source.join("code"), &state, lookup).is_ok());
        assert!(ManagedWorkspace::from_values(&source, &source.join("state/12"), lookup).is_err());
        for missing in ["PROJECT_ID", "USERAPP_WORKSPACE_DIR", "APP_CLI_STATE_ROOT"] {
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
    fn actual_k8s_builder_type_uses_platform_source_despite_old_code_declaration() {
        let (_temp, source, state, env) = fixture();
        let managed = ManagedWorkspace::from_values(&source, &state, |key| match key {
            "SERVICE_TYPE" => Some("user-app-builder".into()),
            "USERAPP_WORKSPACE_DIR" => Some(source.parent().unwrap().as_os_str().to_owned()),
            "APP_CLI_RUNTIME_WORKSPACE" => Some(source.join("code").into_os_string()),
            _ => env
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone()),
        })
        .unwrap()
        .expect("the actual K8s builder declaration must enable recovery");
        assert_eq!(managed.source_root, source);
    }

    #[test]
    fn management_normalization_corrects_both_builder_spellings_without_redirecting_artifacts() {
        let (_temp, source, state, env) = fixture();
        let lookup = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        };
        std::fs::create_dir_all(source.join("code")).unwrap();
        std::fs::create_dir_all(source.join(".run")).unwrap();
        for service in ["user-app-builder", "userapp-builder"] {
            for launch in [&source, &source.join("code"), &source.join(".run")] {
                let actual = normalize_management_workspace_from_values(launch, |key| match key {
                    "SERVICE_TYPE" => Some(service.into()),
                    "APP_CLI_RUNTIME_WORKSPACE" => Some(source.join("code").into_os_string()),
                    _ => lookup(key),
                })
                .unwrap();
                assert_eq!(
                    actual,
                    if launch.file_name().unwrap() == ".run" {
                        launch.clone()
                    } else {
                        source.clone()
                    }
                );
            }
        }
        assert!(normalize_management_workspace_from_values(&source.join("other"), lookup).is_err());
        let managed = ManagedWorkspace::from_values(&source, &state, lookup)
            .unwrap()
            .unwrap();
        assert!(
            managed
                .management_workspace(&source.with_file_name("12"))
                .is_err()
        );
        // Legitimate local deploy aliases preserve their execution directory.
        let artifact = source.join("deploy-output");
        std::fs::create_dir_all(&artifact).unwrap();
        super::super::record_project_origin(&source, &artifact).unwrap();
        assert_eq!(managed.management_workspace(&artifact).unwrap(), artifact);
    }

    #[test]
    fn production_and_standalone_workspace_selection_is_unchanged() {
        for service in [None, Some("user-app"), Some("app-runtime")] {
            let workspace = Path::new("project/code");
            let normalized =
                normalize_management_workspace_from_values(workspace, |key| match key {
                    "SERVICE_TYPE" => service.map(OsString::from),
                    "APP_CLI_MANAGED" => Some("1".into()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(normalized, workspace);
        }
    }

    #[test]
    fn single_application_mode_keeps_the_same_source_contract_as_file_server() {
        let (_temp, source, state, env) = fixture();
        let lookup = |key: &str| match key {
            "USERAPP_WORKSPACE_DIR" => Some(source.clone().into_os_string()),
            "USERAPP_SINGLE_APP_ID" => Some("11".into()),
            "APP_CLI_RUNTIME_WORKSPACE" => Some(source.join("code").into_os_string()),
            _ => env
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone()),
        };
        let managed = ManagedWorkspace::from_values(&source, &state, lookup)
            .unwrap()
            .unwrap();
        assert_eq!(managed.source_root, source);
        assert!(managed.single_application);
        let child_env = managed.launch_environment().unwrap();
        assert!(
            child_env
                .iter()
                .any(|(key, value)| key == "USERAPP_WORKSPACE_DIR"
                    && value == &source.display().to_string())
        );
        assert!(
            child_env
                .iter()
                .any(|(key, value)| key == "USERAPP_SINGLE_APP_ID" && value == "11")
        );
        assert!(
            ManagedWorkspace::from_values(&source, &state, |key| match key {
                "USERAPP_SINGLE_APP_ID" => Some("12".into()),
                _ => lookup(key),
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_code_declaration_cannot_authorize_symlink_escape() {
        let (temp, source, state, env) = fixture();
        let foreign = temp.path().join("12");
        std::fs::create_dir_all(&foreign).unwrap();
        std::os::unix::fs::symlink(&foreign, source.join("code")).unwrap();
        assert!(
            ManagedWorkspace::from_values(&source, &state, |key| match key {
                "APP_CLI_RUNTIME_WORKSPACE" => Some(source.join("code").into_os_string()),
                _ => env
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone()),
            })
            .is_err()
        );
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

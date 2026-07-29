use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{
    DiscoveredProject, LogFormat, LogSource, ManifestError, PingapMode, ProjectKind,
    ProjectManifest, SCHEMA_VERSION, WorkspaceManifest,
};

pub fn parse_workspace(content: &str) -> Result<WorkspaceManifest, ManifestError> {
    let manifest: WorkspaceManifest =
        toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))?;
    validate_workspace(&manifest)?;
    Ok(manifest)
}

pub fn parse_project(content: &str) -> Result<ProjectManifest, ManifestError> {
    let manifest: ProjectManifest =
        toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))?;
    validate_project(&manifest)?;
    Ok(manifest)
}

pub fn validate_workspace(manifest: &WorkspaceManifest) -> Result<(), ManifestError> {
    require_v1(manifest.schema_version)?;
    if manifest.workspace.name.trim().is_empty() {
        return Err(ManifestError::Validation(
            "workspace.name must not be empty".into(),
        ));
    }
    match manifest.pingap.mode {
        PingapMode::Managed if manifest.pingap.config.is_some() => Err(ManifestError::Validation(
            "pingap.config is not allowed in managed mode".into(),
        )),
        PingapMode::Extend | PingapMode::Custom => validate_relative_path(
            manifest.pingap.config.as_deref().unwrap_or_default(),
            "pingap.config",
        ),
        PingapMode::Managed => Ok(()),
    }
}

pub fn validate_project(manifest: &ProjectManifest) -> Result<(), ManifestError> {
    require_v1(manifest.schema_version)?;
    let project = &manifest.project;
    if !is_dns1123_label(&project.service_id) {
        return Err(ManifestError::Validation(format!(
            "project.service_id must be a DNS-1123 label: {}",
            project.service_id
        )));
    }
    if project.name.trim().is_empty() {
        return Err(ManifestError::Validation(
            "project.name must not be empty".into(),
        ));
    }
    validate_argv(&manifest.build.command, "build.command")?;
    validate_argv(&manifest.run.command, "run.command")?;
    if !manifest.run.migrate.is_empty() {
        validate_argv(&manifest.run.migrate, "run.migrate")?;
    }
    validate_relative_path(&manifest.build.artifact, "build.artifact")?;
    if manifest.run.shutdown_timeout_seconds == 0 {
        return Err(ManifestError::Validation(
            "run.shutdown_timeout_seconds must be greater than zero".into(),
        ));
    }
    if project.kind == ProjectKind::Worker && manifest.proxy.is_some() {
        return Err(ManifestError::Validation(
            "worker service must not declare [proxy]".into(),
        ));
    }
    if let Some(proxy) = &manifest.proxy {
        validate_http_path(&proxy.path, "proxy.path")?;
    }
    for (field, path) in [
        ("health.startup_path", &manifest.health.startup_path),
        ("health.readiness_path", &manifest.health.readiness_path),
        ("health.liveness_path", &manifest.health.liveness_path),
    ] {
        validate_http_path(path, field)?;
    }
    validate_logs(&manifest.logs.sources)?;
    for key in manifest.env.keys() {
        if is_reserved_env(key) {
            return Err(ManifestError::Validation(format!(
                "env key is reserved by the runtime: {key}"
            )));
        }
    }
    Ok(())
}

pub fn validate_topology(projects: &[DiscoveredProject]) -> Result<Vec<String>, ManifestError> {
    let enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    if enabled.is_empty() {
        return Err(ManifestError::Validation(
            "workspace has no enabled services".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut routes = BTreeSet::new();
    let mut catch_all: Option<&str> = None;
    for project in &enabled {
        let id = project.service_id();
        if !ids.insert(id.to_owned()) {
            return Err(ManifestError::Validation(format!(
                "duplicate service_id: {id}"
            )));
        }
        if let Some(proxy) = &project.manifest.proxy {
            if !routes.insert(proxy.path.clone()) {
                return Err(ManifestError::Validation(format!(
                    "duplicate proxy path: {}",
                    proxy.path
                )));
            }
            if proxy.path == "/" {
                if let Some(previous) = catch_all {
                    return Err(ManifestError::Validation(format!(
                        "multiple catch-all routes: {previous}, {id}"
                    )));
                }
                catch_all = Some(id);
            }
        }
    }
    for project in &enabled {
        for dependency in &project.manifest.run.depends_on {
            if dependency == project.service_id() {
                return Err(ManifestError::Validation(format!(
                    "service {} depends on itself",
                    project.service_id()
                )));
            }
            if !ids.contains(dependency) {
                return Err(ManifestError::Validation(format!(
                    "service {} depends on missing or disabled service {dependency}",
                    project.service_id()
                )));
            }
        }
    }
    topological_order(&enabled)
}

fn topological_order(projects: &[&DiscoveredProject]) -> Result<Vec<String>, ManifestError> {
    let dependencies: BTreeMap<String, BTreeSet<String>> = projects
        .iter()
        .map(|project| {
            (
                project.service_id().to_owned(),
                project.manifest.run.depends_on.iter().cloned().collect(),
            )
        })
        .collect();
    let mut remaining = dependencies;
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, dependencies)| {
                dependencies
                    .iter()
                    .all(|dependency| result.contains(dependency))
            })
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            let ids = remaining.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(ManifestError::Validation(format!(
                "service dependency cycle detected among: {ids}"
            )));
        }
        for id in ready {
            remaining.remove(&id);
            result.push(id);
        }
    }
    Ok(result)
}

fn validate_logs(sources: &[LogSource]) -> Result<(), ManifestError> {
    let mut ids = BTreeSet::new();
    for source in sources {
        if !is_dns1123_label(&source.id) {
            return Err(ManifestError::Validation(format!(
                "logs source id must be a DNS-1123 label: {}",
                source.id
            )));
        }
        if !ids.insert(&source.id) {
            return Err(ManifestError::Validation(format!(
                "duplicate logs source id: {}",
                source.id
            )));
        }
        validate_relative_path(&source.glob, "logs.sources.glob")?;
        if source.glob.contains('/') || source.glob.contains('\\') {
            return Err(ManifestError::Validation(format!(
                "logs.sources.glob must be relative to the service log directory: {}",
                source.glob
            )));
        }
        if source.format == LogFormat::Jsonl && source.multiline_start_pattern.is_some() {
            return Err(ManifestError::Validation(
                "multiline_start_pattern is only valid for text logs".into(),
            ));
        }
    }
    Ok(())
}

fn validate_argv(argv: &[String], field: &str) -> Result<(), ManifestError> {
    if argv.is_empty() || argv.iter().any(|arg| arg.is_empty() || arg.contains('\0')) {
        return Err(ManifestError::Validation(format!(
            "{field} must be a non-empty argv array without empty/NUL arguments"
        )));
    }
    Ok(())
}

fn validate_relative_path(value: &str, field: &str) -> Result<(), ManifestError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ManifestError::Validation(format!(
            "{field} must be a safe relative path: {value}"
        )));
    }
    Ok(())
}

fn validate_http_path(value: &str, field: &str) -> Result<(), ManifestError> {
    if !value.starts_with('/') || value.contains("..") || value.contains('?') || value.contains('#')
    {
        return Err(ManifestError::Validation(format!(
            "{field} must be an absolute URL path without traversal/query/fragment: {value}"
        )));
    }
    Ok(())
}

fn require_v1(version: u32) -> Result<(), ManifestError> {
    if version != SCHEMA_VERSION {
        return Err(ManifestError::Validation(format!(
            "unsupported schema_version {version}; expected {SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn is_dns1123_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    let valid_edge = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes.first().is_some_and(valid_edge)
        && bytes.last().is_some_and(valid_edge)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn is_reserved_env(key: &str) -> bool {
    matches!(
        key,
        "PORT" | "HOST" | "HOSTNAME" | "APP_LOG_DIR" | "APP_SERVICE_ID" | "APP_RELEASE_ID"
    ) || key.starts_with("RCODER_")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{BuildSection, HealthSection, ProjectMeta, ProjectType, RunSection};

    fn project(id: &str, depends_on: &[&str]) -> ProjectManifest {
        ProjectManifest {
            schema_version: 1,
            project: ProjectMeta {
                service_id: id.into(),
                name: id.into(),
                r#type: ProjectType::Go,
                kind: ProjectKind::Web,
                enabled: true,
            },
            build: BuildSection {
                command: vec!["sh".into(), "build.sh".into()],
                artifact: "artifact.zip".into(),
            },
            run: RunSection {
                command: vec!["./server".into()],
                migrate: Vec::new(),
                depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
                shutdown_timeout_seconds: 30,
            },
            health: HealthSection::default(),
            proxy: None,
            logs: Default::default(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn rejects_unknown_and_legacy_fields() {
        assert!(parse_workspace("[workspace]\nname='old'\n").is_err());
        assert!(parse_workspace("schema_version=1\n[workspace]\nname='x'\nother=true\n").is_err());
    }

    #[test]
    fn validates_dependency_order_and_cycle() {
        let projects = vec![
            DiscoveredProject {
                dir: "api".into(),
                manifest: project("api", &["db"]),
            },
            DiscoveredProject {
                dir: "db".into(),
                manifest: project("db", &[]),
            },
        ];
        assert_eq!(
            validate_topology(&projects).expect("valid topology"),
            vec!["db", "api"]
        );
        let cycle = vec![
            DiscoveredProject {
                dir: "a".into(),
                manifest: project("a", &["b"]),
            },
            DiscoveredProject {
                dir: "b".into(),
                manifest: project("b", &["a"]),
            },
        ];
        assert!(validate_topology(&cycle).is_err());
    }

    #[test]
    fn dns1123_validation_covers_full_label() {
        assert!(is_dns1123_label("backend-go"));
        for invalid in ["Backend", "-backend", "backend-", "back_end", "a.b"] {
            assert!(!is_dns1123_label(invalid), "{invalid}");
        }
    }
}

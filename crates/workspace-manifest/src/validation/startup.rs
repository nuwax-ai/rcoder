//! Startup semantics shared by project manifests and direct release-lock readers.
use super::{ValidationIssue, project::validate_argv_issue};
use crate::{
    DevrunSection, DiscoveredProject, HealthSection, ManifestError,
    PROCESS_STARTUP_OBSERVATION_SECONDS, PingapMode, ProjectKind, ProjectType, ReleaseLock,
    RunSection, StartupProbe, WorkspaceManifest,
};

pub(super) fn collect_startup_issues(
    kind: &ProjectKind,
    project_type: &ProjectType,
    health: &HealthSection,
    has_proxy: bool,
    run: &RunSection,
    devrun: Option<&DevrunSection>,
) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let Some(probe) = health.startup_probe else {
        return issues;
    };
    let mut issue = |message: &str, field: &str| {
        issues.push(ValidationIssue::new(message).at_field(field));
    };
    if health.startup_timeout_seconds == 0 {
        issue(
            "health.startup_timeout_seconds must be greater than zero",
            "health.startup_timeout_seconds",
        );
    }
    if *project_type == ProjectType::Static {
        issue(
            "static services use built-in hosting; startup_probe requires a process service",
            "health.startup_probe",
        );
    }
    if probe == StartupProbe::Process {
        if *kind != ProjectKind::Worker || *project_type == ProjectType::Static || has_proxy {
            issue(
                "process startup probe requires a non-static worker without [proxy]",
                "health.startup_probe",
            );
        }
        if health.startup_timeout_seconds <= PROCESS_STARTUP_OBSERVATION_SECONDS {
            issue(
                "process startup timeout must be greater than the 5 second observation interval",
                "health.startup_timeout_seconds",
            );
        }
        if [
            &health.startup_path,
            &health.readiness_path,
            &health.liveness_path,
        ]
        .iter()
        .any(|path| path.as_str() != "/health")
        {
            issue(
                "process startup probe must not declare HTTP health paths; remove the path fields",
                "health.startup_probe",
            );
        }
    }
    if *project_type != ProjectType::Static
        && let Some(issue) = validate_argv_issue(&run.command, "run.command")
    {
        issues.push(issue.at_field("run.command"));
    }
    if let Some(devrun) = devrun
        && let Some(issue) = validate_argv_issue(&devrun.command, "devrun.command")
    {
        issues.push(issue.at_field("devrun.command"));
    }
    issues
}

/// 跨服务 startup 规则的收集入口；调用方须先确认所有项目 manifest 可用。
pub fn collect_workspace_startup_issues(
    workspace: &WorkspaceManifest,
    projects: &[DiscoveredProject],
) -> Vec<ValidationIssue> {
    collect_workspace_rules(
        &workspace.pingap.mode,
        workspace.health.bridge_service.as_deref(),
        projects
            .iter()
            .filter(|p| p.manifest.project.enabled)
            .map(|p| {
                (
                    p.service_id(),
                    &p.manifest.project.kind,
                    p.manifest.health.startup_probe,
                )
            }),
    )
}

/// Called before builds as well as before locking. Discovery alone has no workspace mode.
pub fn validate_workspace_startup(
    workspace: &WorkspaceManifest,
    projects: &[DiscoveredProject],
) -> Result<(), ManifestError> {
    collect_workspace_startup_issues(workspace, projects)
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.message))
        .map_or(Ok(()), Err)
}

pub fn validate_release_startup(lock: &ReleaseLock) -> Result<(), ManifestError> {
    for service in &lock.services {
        let issues = collect_startup_issues(
            &service.kind,
            &service.r#type,
            &service.health,
            service.proxy.is_some(),
            &service.run,
            service.devrun.as_ref(),
        );
        if !issues.is_empty() {
            return Err(ManifestError::Validation(format!(
                "service {}: {}",
                service.service_id,
                issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
    }
    validate_workspace_rules(
        &lock.pingap.mode,
        lock.bridge_service.as_deref(),
        lock.services
            .iter()
            .filter(|s| s.enabled)
            .map(|s| (s.service_id.as_str(), &s.kind, s.health.startup_probe)),
    )
}

/// A new writer must not let an old owner stop services before discovering an
/// unsupported startup contract. Absence of the optional field stays compatible.
pub fn require_startup_probe_capability(
    lock: &ReleaseLock,
    capabilities: &[String],
) -> Result<(), ManifestError> {
    if lock
        .services
        .iter()
        .any(|s| s.enabled && s.health.startup_probe.is_some())
        && !capabilities
            .iter()
            .any(|c| c == crate::STARTUP_PROBE_CAPABILITY)
    {
        return Err(ManifestError::Validation(format!(
            "runtime owner lacks {}; upgrade app-cli before starting this release",
            crate::STARTUP_PROBE_CAPABILITY,
        )));
    }
    Ok(())
}

fn validate_workspace_rules<'a>(
    mode: &PingapMode,
    bridge: Option<&str>,
    services: impl Iterator<Item = (&'a str, &'a ProjectKind, Option<StartupProbe>)>,
) -> Result<(), ManifestError> {
    collect_workspace_rules(mode, bridge, services)
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.message))
        .map_or(Ok(()), Err)
}

fn collect_workspace_rules<'a>(
    mode: &PingapMode,
    bridge: Option<&str>,
    services: impl Iterator<Item = (&'a str, &'a ProjectKind, Option<StartupProbe>)>,
) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let mut workers = 0;
    let mut webs = 0;
    for (id, kind, probe) in services {
        if probe == Some(StartupProbe::Process) && bridge == Some(id) {
            issues.push(
                ValidationIssue::new(format!(
                    "bridge_service must not reference process worker '{id}'"
                ))
                .at_file("workspace.manifest.toml")
                .at_service(id)
                .at_field("health.bridge_service")
                .with_hint(
                    "remove bridge_service or reference a service with an HTTP health endpoint",
                ),
            );
        }
        match kind {
            ProjectKind::Worker => workers += 1,
            ProjectKind::Web => webs += 1,
        }
    }
    if *mode == PingapMode::Managed && workers > 0 && webs == 0 {
        issues.push(
            ValidationIssue::new(
                "managed mode requires a web service; a worker-only workspace is not supported",
            )
            .at_file("workspace.manifest.toml")
            .at_field("pingap.mode")
            .with_hint("add an enabled web service or select extend/custom mode with a valid configuration"),
        );
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_reports_independent_workspace_startup_rules_and_preserves_first_error() {
        let workspace = crate::parse_workspace(
            "schema_version=1\n[workspace]\nname='workers'\n[health]\nbridge_service='worker'\n",
        )
        .unwrap();
        let worker = crate::parse_project(
            "schema_version=1\n[project]\nservice_id='worker'\nname='worker'\n\
             type='node'\nkind='worker'\n[build]\ncommand=['build']\nartifact='artifact.zip'\n\
             [run]\ncommand=['run']\n[health]\nstartup_probe='process'\n",
        )
        .unwrap();
        let projects = vec![DiscoveredProject {
            dir: "worker".into(),
            manifest: worker,
        }];

        let issues = collect_workspace_startup_issues(&workspace, &projects);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].field.as_deref(), Some("health.bridge_service"));
        assert_eq!(issues[1].field.as_deref(), Some("pingap.mode"));
        let error = validate_workspace_startup(&workspace, &projects).unwrap_err();
        assert!(
            matches!(error, ManifestError::Validation(message) if message == issues[0].message)
        );
    }
}

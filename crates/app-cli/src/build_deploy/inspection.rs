//! Read source configuration using the same manifest and proxy rules as gen-lock.
use std::path::Path;

use serde::Serialize;
use workspace_manifest::{
    DiagnosticKind, DiscoveredProject, ManifestDiagnostic, ManifestError, ReleaseLock,
    ReleaseMetadata, ValidationIssue, build_release_lock, collect_workspace_issues,
    collect_workspace_startup_issues, discover_projects_report, parse_workspace_for_inspection,
};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Parse,
    Validation,
    Io,
    Internal,
}

impl From<DiagnosticKind> for IssueKind {
    fn from(value: DiagnosticKind) -> Self {
        match value {
            DiagnosticKind::Parse => Self::Parse,
            DiagnosticKind::Validation => Self::Validation,
            DiagnosticKind::Io => Self::Io,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Diagnostic {
    pub kind: IssueKind,
    #[serde(flatten)]
    pub issue: ValidationIssue,
}

#[derive(Debug, Serialize)]
pub struct ServiceSummary {
    pub service_id: String,
    pub dir: String,
    pub port: Option<u16>,
    pub proxy_path: Option<String>,
    pub effective_strip_prefix: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct InspectionReport {
    pub report_version: u32,
    pub scope: &'static str,
    pub profile: &'static str,
    pub valid: bool,
    pub topology_checked: bool,
    pub services_complete: bool,
    pub services: Vec<ServiceSummary>,
    pub diagnostics: Vec<Diagnostic>,
    pub not_checked: Vec<&'static str>,
    pub skipped_checks: Vec<&'static str>,
}

impl InspectionReport {
    fn empty(dev: bool) -> Self {
        Self {
            report_version: 1,
            scope: "configuration",
            profile: if dev { "dev" } else { "prod" },
            valid: false,
            topology_checked: false,
            services_complete: false,
            services: Vec::new(),
            diagnostics: Vec::new(),
            not_checked: vec![
                "build_execution",
                "artifact_contents",
                "port_availability",
                "service_readiness",
                "database_connectivity",
                "runtime_owner_capabilities",
                "upstream_reachability",
            ],
            skipped_checks: Vec::new(),
        }
    }

    pub(crate) fn unavailable(dev: bool, kind: IssueKind, message: &str) -> Self {
        let mut report = Self::empty(dev);
        report.issue(kind, ValidationIssue::new(message));
        report
            .skipped_checks
            .extend(["manifest", "topology", "locking", "proxy"]);
        report
    }

    pub fn exit_code(&self) -> i32 {
        if self.valid {
            0
        } else if self
            .diagnostics
            .iter()
            .any(|item| matches!(item.kind, IssueKind::Io | IssueKind::Internal))
        {
            3
        } else {
            1
        }
    }

    fn issue(&mut self, kind: IssueKind, issue: ValidationIssue) {
        self.diagnostics.push(Diagnostic { kind, issue });
    }
}

/// Internal results are deliberately not serialized: they contain env and proxy configuration.
pub(crate) struct Inspection {
    pub report: InspectionReport,
    pub projects: Vec<DiscoveredProject>,
    pub lock: Option<ReleaseLock>,
    pub proxy: Option<(String, String)>,
}

#[derive(Clone, Copy)]
pub(crate) enum InspectionPurpose {
    Validate,
    GenLock,
}

pub(crate) async fn inspect(
    workspace: &Path,
    dev: bool,
    metadata: ReleaseMetadata<'_>,
    purpose: InspectionPurpose,
) -> Inspection {
    let mut result = Inspection {
        report: InspectionReport::empty(dev),
        projects: Vec::new(),
        lock: None,
        proxy: None,
    };
    match tokio::fs::metadata(workspace).await {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            result.report.issue(
                IssueKind::Io,
                ValidationIssue::new("workspace path is not a directory")
                    .at_file(workspace.display().to_string()),
            );
            result
                .report
                .skipped_checks
                .extend(["manifest", "topology", "locking", "proxy"]);
            return result;
        }
        Err(error) => {
            result.report.issue(
                IssueKind::Io,
                ValidationIssue::new(format!("cannot observe workspace: {error}"))
                    .at_file(workspace.display().to_string()),
            );
            result
                .report
                .skipped_checks
                .extend(["manifest", "topology", "locking", "proxy"]);
            return result;
        }
    }
    let ws_file = "workspace.manifest.toml";
    let ws_manifest = match tokio::fs::read_to_string(workspace.join(ws_file)).await {
        Ok(content) => match parse_workspace_for_inspection(&content, ws_file) {
            Ok(manifest) => {
                for issue in collect_workspace_issues(&manifest, ws_file) {
                    result.report.issue(IssueKind::Validation, issue);
                }
                Some(manifest)
            }
            Err(issue) => {
                result.report.issue(IssueKind::Parse, issue);
                None
            }
        },
        Err(error) => {
            result.report.issue(
                IssueKind::Io,
                ValidationIssue::new(format!("cannot read workspace manifest: {error}"))
                    .at_file(ws_file),
            );
            None
        }
    };
    let discovery_root = workspace.to_path_buf();
    match tokio::task::spawn_blocking(move || discover_projects_report(&discovery_root)).await {
        Ok(Ok(report)) => {
            result.report.topology_checked = report.topology_checked;
            result.report.services_complete = report.topology_checked;
            result.projects = report.projects;
            for ManifestDiagnostic { kind, issue } in report.diagnostics {
                result.report.issue(kind.into(), issue);
            }
        }
        Ok(Err(error)) => result.report.issue(
            IssueKind::Io,
            ValidationIssue::new(error.to_string()).at_file(workspace.display().to_string()),
        ),
        Err(_) => result.report.issue(
            IssueKind::Internal,
            ValidationIssue::new("manifest discovery task failed")
                .at_file(workspace.display().to_string()),
        ),
    }
    for project in result
        .projects
        .iter()
        .filter(|p| p.manifest.project.enabled)
    {
        let proxy = project.manifest.proxy.as_ref();
        result.report.services.push(ServiceSummary {
            service_id: project.service_id().to_owned(),
            dir: project.dir.clone(),
            port: None,
            proxy_path: proxy.map(|p| p.path.clone()),
            effective_strip_prefix: proxy
                .map(|p| p.effective_strip_prefix(dev && project.manifest.devrun.is_some())),
        });
    }
    if !result.report.topology_checked {
        result.report.skipped_checks.push("topology");
    }
    if !result.report.diagnostics.is_empty() {
        result.report.skipped_checks.extend(["locking", "proxy"]);
        return result;
    }
    let Some(manifest) = ws_manifest else {
        result.report.issue(
            IssueKind::Internal,
            ValidationIssue::new("workspace parse produced no result"),
        );
        result.report.skipped_checks.extend(["locking", "proxy"]);
        return result;
    };
    for issue in collect_workspace_startup_issues(&manifest, &result.projects) {
        result
            .report
            .issue(IssueKind::Validation, issue.at_file(ws_file));
    }
    if !result.report.diagnostics.is_empty() {
        result.report.skipped_checks.extend(["locking", "proxy"]);
        return result;
    }
    let lock = match build_release_lock(&manifest, &result.projects, metadata) {
        Ok(lock) => lock,
        Err(error) => {
            let kind = match error {
                ManifestError::Validation(_) => IssueKind::Validation,
                ManifestError::Parse(_) => IssueKind::Internal,
            };
            result.report.issue(
                kind,
                ValidationIssue::new(error.to_string()).at_file(ws_file),
            );
            result.report.skipped_checks.push("proxy");
            return result;
        }
    };
    result.report.services = lock
        .services
        .iter()
        .map(|service| {
            let proxy = service.proxy.as_ref();
            ServiceSummary {
                service_id: service.service_id.clone(),
                dir: service.dir.clone(),
                port: Some(service.port),
                proxy_path: proxy.map(|p| p.path.clone()),
                effective_strip_prefix: proxy
                    .map(|p| p.effective_strip_prefix(dev && service.devrun.is_some())),
            }
        })
        .collect();
    let compiled = match purpose {
        InspectionPurpose::Validate => {
            let proxy_workspace = workspace.to_path_buf();
            let proxy_lock = lock.clone();
            tokio::spawn(async move {
                crate::proxy::compiler::compile_effective_config_for_inspection(
                    &proxy_workspace,
                    &proxy_lock,
                    dev,
                )
                .await
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        }
        InspectionPurpose::GenLock => {
            crate::proxy::compiler::compile_effective_config(workspace, &lock, dev).await
        }
    };
    match compiled {
        Ok(proxy) => result.proxy = Some(proxy),
        Err(error) => {
            if let Some(source) = error.chain().find_map(|cause| {
                cause.downcast_ref::<crate::proxy::config_source::ConfigSourceError>()
            }) {
                result
                    .report
                    .issue(source.kind.into(), source.issue.as_ref().clone());
                result.lock = Some(lock);
                return result;
            }
            // Native I/O sources survive anyhow contexts; do not classify translated messages.
            let io = error
                .chain()
                .find_map(|source| source.downcast_ref::<std::io::Error>());
            let (kind, message) = if let Some(io) = io {
                (
                    IssueKind::Io,
                    format!("cannot observe proxy configuration: {io}"),
                )
            } else if error
                .chain()
                .any(|cause| cause.downcast_ref::<tokio::task::JoinError>().is_some())
            {
                (
                    IssueKind::Internal,
                    "proxy validator task failed".to_owned(),
                )
            } else {
                (
                    IssueKind::Validation,
                    "proxy configuration failed syntax, service-reference, or guardrail validation"
                        .to_owned(),
                )
            };
            result.report.issue(kind, ValidationIssue::new(message)
                .at_file(manifest.pingap.config.as_deref().unwrap_or(ws_file))
                .at_field("pingap")
                .with_hint("check proxy mode, service routes/references, and the configured Pingap file; no runtime was started"));
        }
    }
    result.lock = Some(lock);
    result.report.valid = result.report.diagnostics.is_empty();
    result
}

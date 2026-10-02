//! CLI presentation for read-only source configuration inspection.
use std::io::{self, Write};
use std::path::Path;

use workspace_manifest::ReleaseMetadata;

use super::inspection::{InspectionPurpose, InspectionReport, IssueKind};

const INSPECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn run(workspace: &Path, dev: bool, json: bool) -> io::Result<i32> {
    let (version, commit) = super::devtool::pingap_identity();
    let inspection = super::inspection::inspect(
        workspace,
        dev,
        ReleaseMetadata {
            release_id: "00000000000000000000000000000000",
            pingap_version: &version,
            pingap_commit: &commit,
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: "validation-preview",
        },
        InspectionPurpose::Validate,
    );
    let report = match tokio::time::timeout(INSPECTION_TIMEOUT, inspection).await {
        Ok(result) => result.report,
        Err(_) => InspectionReport::unavailable(
            dev,
            IssueKind::Io,
            "configuration inspection exceeded its 30 second observation budget",
        ),
    };
    write_report(&report, json)?;
    Ok(report.exit_code())
}

pub fn write_initialization_failure(_workspace: &Path, dev: bool, json: bool) -> io::Result<()> {
    write_report(
        &InspectionReport::unavailable(
            dev,
            IssueKind::Internal,
            "cannot initialize the configuration validation executor",
        ),
        json,
    )
}

fn write_report(report: &InspectionReport, json: bool) -> io::Result<()> {
    let mut output = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
        writeln!(output)?;
    } else {
        write_text(&mut output, report)?;
    }
    output.flush()
}

pub(crate) fn write_text(output: &mut impl Write, report: &InspectionReport) -> io::Result<()> {
    writeln!(output, "Configuration validation ({})", report.profile)?;
    for item in &report.diagnostics {
        writeln!(output, "- {:?}: {}", item.kind, item.issue)?;
    }
    if !report.services_complete {
        writeln!(
            output,
            "Service list is partial because one or more inputs could not be checked."
        )?;
    }
    for service in &report.services {
        writeln!(
            output,
            "- service={} dir={} port={:?} route={:?}",
            service.service_id, service.dir, service.port, service.proxy_path
        )?;
    }
    writeln!(
        output,
        "{}: configuration only; no builds or services were executed.",
        if report.valid { "Passed" } else { "Not passed" }
    )?;
    if !report.skipped_checks.is_empty() {
        writeln!(
            output,
            "Skipped checks: {}",
            report.skipped_checks.join(", ")
        )?;
    }
    writeln!(output, "Not checked: {}", report.not_checked.join(", "))
}

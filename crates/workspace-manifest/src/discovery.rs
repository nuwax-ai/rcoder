use std::path::Path;

use crate::{
    DiagnosticKind, DiscoverError, DiscoveredProject, ManifestDiagnostic, ProjectManifest,
    ValidationIssue, collect_topology_issues, manifest_file_of, parse_project, validate_project_at,
    validate_topology, validation::parse_project_for_inspection,
};

/// 扫描 workspace 根的一级子目录，发现并解析所有 `project.manifest.toml`。
///
/// 文件系统层：只负责"找目录 + 读文件 + 解析"；解析后的排序与拓扑校验交给
/// [`assemble_discovered`]——后者与文件系统无关，可被"重锁"（从 zip 内 manifest
/// 重建 release.lock.toml）等无文件系统场景复用。
pub fn discover_projects(ws_root: &Path) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<(String, ProjectManifest)> = Vec::new();
    for entry in std::fs::read_dir(ws_root).map_err(|error| DiscoverError::ReadDir {
        path: ws_root.display().to_string(),
        source: error.to_string(),
    })? {
        let entry = entry.map_err(|error| DiscoverError::Io(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| DiscoverError::Io(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let dir = entry.file_name().to_string_lossy().to_string();
        let path = entry.path().join("project.manifest.toml");
        if !path.is_file() {
            continue;
        }
        let content =
            std::fs::read_to_string(&path).map_err(|error| DiscoverError::ReadManifest {
                path: path.display().to_string(),
                source: error.to_string(),
            })?;
        let manifest = parse_project(&content).map_err(|error| DiscoverError::ParseManifest {
            path: path.display().to_string(),
            source: error.to_string(),
        })?;
        discovered.push((dir, manifest));
    }
    assemble_discovered(discovered)
}

/// 装配已解析的项目集合：按 `service_id` 排序 + 拓扑校验。
///
/// 与文件系统无关。既供 [`discover_projects`] 复用，也供未来 Stage 2 的
/// `relock_from_package`（从版本包 zip 内的 manifest 重锁，无文件系统访问）复用。
pub fn assemble_discovered(
    projects: Vec<(String, ProjectManifest)>,
) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<DiscoveredProject> = projects
        .into_iter()
        .map(|(dir, manifest)| DiscoveredProject { dir, manifest })
        .collect();
    discovered.sort_by(|a, b| a.service_id().cmp(b.service_id()));
    validate_topology(&discovered).map_err(|error| DiscoverError::Validation(error.to_string()))?;
    Ok(discovered)
}

/// 发现报告只保留通过单文件校验的项目，并保留每条诊断的来源类型。
#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub projects: Vec<DiscoveredProject>,
    pub diagnostics: Vec<ManifestDiagnostic>,
    /// true 表示完整单文件输入已用于跨服务拓扑检查，即使拓扑本身不合法。
    pub topology_checked: bool,
}

/// 收集一级服务目录的配置问题；不执行命令、不写配置。
///
/// 没有 manifest 的目录与目录符号链接不会成为服务。存在的 manifest 必须
/// 是可读取的普通文件；目录条目、文件类型和读取失败均保留为 I/O 诊断。
/// 任一模块输入不可用时不检查拓扑，避免将未读到的服务误报为缺失依赖。
pub fn discover_projects_report(ws_root: &Path) -> Result<DiscoveryReport, DiscoverError> {
    let directory = std::fs::read_dir(ws_root).map_err(|error| DiscoverError::ReadDir {
        path: ws_root.display().to_string(),
        source: error.to_string(),
    })?;
    let mut diagnostics = Vec::new();
    let mut entries = Vec::new();
    for entry in directory {
        match entry {
            Ok(entry) => {
                let dir = entry.file_name().to_string_lossy().to_string();
                entries.push((dir, entry));
            }
            Err(error) => diagnostics.push(io_diagnostic(
                &ws_root.display().to_string(),
                "cannot read directory entry",
                &error,
            )),
        }
    }
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut projects = Vec::new();
    for (dir, entry) in entries {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                diagnostics.push(io_diagnostic(
                    &dir,
                    "cannot inspect directory entry",
                    &error,
                ));
                continue;
            }
        }
        let path = entry.path().join("project.manifest.toml");
        let file = manifest_file_of(&dir);
        // symlink_metadata 区分不存在的 manifest 与存在但目标不可用的符号链接。
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(io_diagnostic(&file, "cannot inspect manifest", &error));
                continue;
            }
        };
        let metadata = if metadata.file_type().is_symlink() {
            match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    diagnostics.push(io_diagnostic(
                        &file,
                        "cannot inspect manifest target",
                        &error,
                    ));
                    continue;
                }
            }
        } else {
            metadata
        };
        if !metadata.is_file() {
            diagnostics.push(ManifestDiagnostic {
                kind: DiagnosticKind::Io,
                issue: ValidationIssue::new("manifest must be a regular file")
                    .at_file(&file)
                    .with_hint("replace this path with a readable project.manifest.toml file"),
            });
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                diagnostics.push(io_diagnostic(&file, "cannot read manifest", &error));
                continue;
            }
        };
        let manifest = match parse_project_for_inspection(&content, &file) {
            Ok(manifest) => manifest,
            Err(issue) => {
                diagnostics.push(ManifestDiagnostic {
                    kind: DiagnosticKind::Parse,
                    issue,
                });
                continue;
            }
        };
        let file_issues = validate_project_at(&manifest, &dir);
        if file_issues.is_empty() {
            projects.push(DiscoveredProject { dir, manifest });
        } else {
            diagnostics.extend(file_issues.into_iter().map(|issue| ManifestDiagnostic {
                kind: DiagnosticKind::Validation,
                issue,
            }));
        }
    }
    projects.sort_by(|left, right| left.service_id().cmp(right.service_id()));
    let topology_checked = diagnostics.is_empty();
    if topology_checked {
        diagnostics.extend(collect_topology_issues(&projects).into_iter().map(|issue| {
            ManifestDiagnostic {
                kind: DiagnosticKind::Validation,
                issue,
            }
        }));
    }
    Ok(DiscoveryReport {
        projects,
        diagnostics,
        topology_checked,
    })
}

fn io_diagnostic(file: &str, operation: &str, error: &std::io::Error) -> ManifestDiagnostic {
    ManifestDiagnostic {
        kind: DiagnosticKind::Io,
        issue: ValidationIssue::new(format!("{operation}: {error}"))
            .at_file(file)
            .with_hint("check the path, file type, encoding, and read permissions, then re-run"),
    }
}

/// 兼容宽松入口：保留签名，将 typed 诊断映射为原来的 issue 清单。
///
/// 与 [`discover_projects_report`] 共用发现和拓扑检查；有不可用模块时不会对
/// 部分输入产生“缺失依赖”的衍生错误。
pub fn discover_projects_lenient(
    ws_root: &Path,
) -> Result<(Vec<DiscoveredProject>, Vec<ValidationIssue>), DiscoverError> {
    let report = discover_projects_report(ws_root)?;
    Ok((
        report.projects,
        report
            .diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.issue)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_toml(id: &str, schema_version: u32, dependencies: &[&str]) -> String {
        let dependencies = dependencies
            .iter()
            .map(|dependency| format!("'{dependency}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "schema_version = {schema_version}\n\
             [project]\nservice_id = '{id}'\nname = '{id}'\ntype = 'node'\n\
             [build]\ncommand = ['missing-build-command']\nartifact = 'artifact.zip'\n\
             [run]\ncommand = ['missing-run-command']\ndepends_on = [{dependencies}]\n"
        )
    }

    fn write_project(root: &Path, dir: &str, content: &[u8]) {
        let path = root.join(dir);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("project.manifest.toml"), content).unwrap();
    }

    #[test]
    fn mixed_io_and_semantic_issues_do_not_invent_missing_dependencies() {
        let root = tempfile::tempdir().unwrap();
        write_project(root.path(), "unreadable", &[0xff, 0xfe]);
        write_project(
            root.path(),
            "invalid",
            project_toml("invalid", 2, &[]).as_bytes(),
        );
        write_project(
            root.path(),
            "frontend",
            project_toml("frontend", 1, &["unreadable", "invalid"]).as_bytes(),
        );

        let report = discover_projects_report(root.path()).unwrap();
        assert!(!report.topology_checked);
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.projects[0].service_id(), "frontend");
        assert_eq!(report.diagnostics.len(), 2);
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::Io
                && diagnostic.issue.file.as_deref() == Some("unreadable/project.manifest.toml")
        }));
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::Validation
                && diagnostic.issue.file.as_deref() == Some("invalid/project.manifest.toml")
                && diagnostic.issue.field.as_deref() == Some("schema_version")
        }));
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| { diagnostic.issue.field.as_deref() != Some("run.depends_on") })
        );

        let (projects, issues) = discover_projects_lenient(root.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(issues.len(), 2);
        assert!(
            issues
                .iter()
                .all(|issue| issue.field.as_deref() != Some("run.depends_on"))
        );
    }

    #[test]
    fn parse_diagnostic_keeps_location_without_disclosing_source_values() {
        let root = tempfile::tempdir().unwrap();
        let content = project_toml("frontend", 1, &[])
            .replace("type = 'node'", "type = 'DO_NOT_DISCLOSE_SECRET'");
        write_project(root.path(), "frontend", content.as_bytes());

        let report = discover_projects_report(root.path()).unwrap();
        assert!(!report.topology_checked);
        assert!(report.projects.is_empty());
        assert_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.kind, DiagnosticKind::Parse);
        assert_eq!(
            diagnostic.issue.file.as_deref(),
            Some("frontend/project.manifest.toml")
        );
        let rendered = diagnostic.issue.to_string();
        assert!(rendered.contains("line 5, column "));
        assert!(!rendered.contains("DO_NOT_DISCLOSE_SECRET"));
        assert!(!rendered.contains("type ="));
    }

    #[test]
    fn non_file_manifest_is_an_io_diagnostic_and_absent_manifest_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("docs")).unwrap();
        std::fs::create_dir_all(root.path().join("invalid/project.manifest.toml")).unwrap();
        write_project(
            root.path(),
            "frontend",
            project_toml("frontend", 1, &[]).as_bytes(),
        );

        let report = discover_projects_report(root.path()).unwrap();
        assert!(!report.topology_checked);
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].kind, DiagnosticKind::Io);
        assert_eq!(
            report.diagnostics[0].issue.file.as_deref(),
            Some("invalid/project.manifest.toml")
        );
    }

    #[test]
    fn complete_inputs_collect_topology_issues_and_record_checked_stage() {
        let root = tempfile::tempdir().unwrap();
        for id in ["first", "second"] {
            write_project(
                root.path(),
                id,
                format!("{}[proxy]\npath = '/api'\n", project_toml(id, 1, &[])).as_bytes(),
            );
        }
        std::fs::create_dir(root.path().join("packages")).unwrap();

        let report = discover_projects_report(root.path()).unwrap();
        assert!(report.topology_checked);
        assert_eq!(report.projects.len(), 2);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].kind, DiagnosticKind::Validation);
        assert_eq!(
            report.diagnostics[0].issue.field.as_deref(),
            Some("proxy.path")
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_ignored_but_broken_manifest_links_are_reported() {
        let root = tempfile::tempdir().unwrap();
        write_project(
            root.path(),
            "frontend",
            project_toml("frontend", 1, &[]).as_bytes(),
        );
        std::os::unix::fs::symlink(root.path().join("frontend"), root.path().join("alias"))
            .unwrap();
        std::fs::create_dir(root.path().join("broken")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("missing"),
            root.path().join("broken/project.manifest.toml"),
        )
        .unwrap();

        let report = discover_projects_report(root.path()).unwrap();
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.projects[0].dir, "frontend");
        assert!(!report.topology_checked);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].kind, DiagnosticKind::Io);
        assert_eq!(
            report.diagnostics[0].issue.file.as_deref(),
            Some("broken/project.manifest.toml")
        );
    }
}

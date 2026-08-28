//! manifest 校验（目录化：issue/parse/project/topology 按域分组；
//! lib.rs 的 pub use validation::{...} 11 符号路径零改动）。

mod issue;
mod parse;
mod project;
mod topology;

pub use issue::{ValidationIssue, manifest_file_of};
pub use parse::{
    parse_project, parse_project_toml, parse_workspace, validate_project, validate_workspace,
};
pub use project::{collect_workspace_issues, validate_project_at, validate_service_id};
pub use topology::{collect_topology_issues, validate_topology};

#[cfg(test)]
mod tests {
    use super::project::is_dns1123_label;
    use crate::{
        DiscoveredProject, LogFormat, ManifestError, ProjectKind, ProjectManifest, SCHEMA_VERSION,
        WorkspaceManifest,
    };

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

    /// 依赖目标缺失/被剔除时不得产出"伪环"：lenient 全量呈现里，缺依赖已由
    /// 专属 issue 报告（含修复指引），依赖它的服务自身无环——再报
    /// "cycle detected" 会误导用户/agent 去改不存在的环。
    #[test]
    fn missing_dependency_does_not_report_fake_cycle() {
        let projects = vec![
            DiscoveredProject {
                dir: "api".into(),
                manifest: project("api", &["db"]),
            },
            // db 缺失（如 lenient 模式下因自身校验错误被剔除）
        ];
        let issues = collect_topology_issues(&projects);
        let fake = issues
            .iter()
            .any(|issue| issue.to_string().contains("cycle"));
        assert!(
            !fake,
            "missing dependency must not fabricate a cycle: {issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.to_string().contains("missing or disabled")),
            "missing-dependency issue must still be reported"
        );
    }

    #[test]
    fn rejects_reserved_postgres_env_keys() {
        for key in ["POSTGRES_USER", "POSTGRES_PASSWORD", "POSTGRES_DB"] {
            let mut manifest = project("web", &[]);
            manifest.env.insert(key.into(), "value".into());
            match validate_project(&manifest) {
                Err(ManifestError::Validation(message)) => assert!(
                    message.contains("reserved by the runtime"),
                    "{key}: {message}"
                ),
                other => panic!("expected reserved env error for {key}, got {other:?}"),
            }
        }
    }

    #[test]
    fn allows_pghost_and_pgport_env_keys() {
        let mut manifest = project("web", &[]);
        manifest.env.insert("PGHOST".into(), "localhost".into());
        manifest.env.insert("PGPORT".into(), "5432".into());
        validate_project(&manifest).expect("PGHOST/PGPORT are not reserved");
    }

    #[test]
    fn duplicate_proxy_path_names_both_modules_and_files() {
        let mut java_a = project("java-a", &[]);
        java_a.proxy = Some(crate::ProxySection {
            path: "/api/java/".into(),
            strip_prefix: true,
            plugins: Vec::new(),
            upstream_includes: Vec::new(),
        });
        let mut java_b = project("java-b", &[]);
        java_b.proxy = Some(crate::ProxySection {
            path: "/api/java/".into(),
            strip_prefix: true,
            plugins: Vec::new(),
            upstream_includes: Vec::new(),
        });
        let projects = vec![
            DiscoveredProject {
                dir: "backend-java-a".into(),
                manifest: java_a,
            },
            DiscoveredProject {
                dir: "backend-java-b".into(),
                manifest: java_b,
            },
        ];
        let issues = collect_topology_issues(&projects);
        assert_eq!(issues.len(), 1, "{issues:?}");
        let rendered = issues[0].to_string();
        for expected in [
            "\"/api/java/\"",
            "java-a",
            "java-b",
            "backend-java-a/project.manifest.toml",
            "backend-java-b/project.manifest.toml",
            "/java-a/",
            "/java-b/",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected} in:\n{rendered}"
            );
        }
        // fast-fail 兼容入口同样带定位（构建链共用此函数）。
        let err = validate_topology(&projects).unwrap_err().to_string();
        assert!(
            err.contains("backend-java-a") && err.contains("backend-java-b"),
            "{err}"
        );
    }

    #[test]
    fn collect_all_reports_every_issue_at_once() {
        let mut bad = project("Bad_ID", &["ghost"]);
        bad.build.command = Vec::new();
        bad.env.insert("POSTGRES_PASSWORD".into(), "x".into());
        let issues = validate_project_at(&bad, "dir-bad");
        let fields: Vec<&str> = issues.iter().filter_map(|i| i.field.as_deref()).collect();
        for expected in [
            "project.service_id",
            "build.command",
            "env.POSTGRES_PASSWORD",
        ] {
            assert!(
                fields.contains(&expected),
                "missing {expected} in {fields:?}"
            );
        }
        // depends_on 的存在性属拓扑域：单文件校验不管，跨服务收集时带文件定位报出。
        let topology = collect_topology_issues(&[DiscoveredProject {
            dir: "dir-bad".into(),
            manifest: bad.clone(),
        }]);
        assert!(
            topology
                .iter()
                .any(|issue| issue.field.as_deref() == Some("run.depends_on")
                    && issue.message.contains("ghost")
                    && issue.file.as_deref() == Some("dir-bad/project.manifest.toml"))
        );
        assert!(issues.iter().all(|issue| {
            issue
                .file
                .as_deref()
                .is_some_and(|file| file == "dir-bad/project.manifest.toml")
        }));
    }

    #[test]
    fn multiple_catch_all_lists_both_services() {
        let mut root_a = project("root-a", &[]);
        root_a.proxy = Some(crate::ProxySection {
            path: "/".into(),
            strip_prefix: false,
            plugins: Vec::new(),
            upstream_includes: Vec::new(),
        });
        let mut root_b = project("root-b", &[]);
        root_b.proxy = Some(crate::ProxySection {
            path: "/".into(),
            strip_prefix: false,
            plugins: Vec::new(),
            upstream_includes: Vec::new(),
        });
        let projects = vec![
            DiscoveredProject {
                dir: "a".into(),
                manifest: root_a,
            },
            DiscoveredProject {
                dir: "b".into(),
                manifest: root_b,
            },
        ];
        let rendered = collect_topology_issues(&projects)
            .iter()
            .map(|issue| issue.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("catch-all")
                && rendered.contains("root-a")
                && rendered.contains("root-b")
        );
    }

    #[test]
    fn dns1123_validation_covers_full_label() {
        assert!(is_dns1123_label("backend-go"));
        for invalid in ["Backend", "-backend", "backend-", "back_end", "a.b"] {
            assert!(!is_dns1123_label(invalid), "{invalid}");
        }
    }
}

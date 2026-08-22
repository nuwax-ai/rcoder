use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{
    DiscoveredProject, LogFormat, LogSource, ManifestError, PingapMode, ProjectKind,
    ProjectManifest, SCHEMA_VERSION, WorkspaceManifest,
};

/// 一条结构化校验问题：面向"用户/agent 拿到即可修复"渲染。
///
/// 渲染格式固定四段（缺失段自动跳过），agent 可直接按 `file → 字段 → 建议`
/// 执行修复；`service`/`field` 均为 TOML 内真实键路径。
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// 出错的 manifest 文件相对路径（如 `backend-java-b/project.manifest.toml`）。
    pub file: Option<String>,
    /// 涉及服务的 `service_id`（跨服务冲突时为首个声明方）。
    pub service: Option<String>,
    /// TOML 字段键路径（如 `proxy.path`、`run.depends_on`）。
    pub field: Option<String>,
    /// 问题陈述（含实际值）。
    pub message: String,
    /// 修复建议（可直接执行的动作或示例值）。
    pub hint: Option<String>,
}

impl ValidationIssue {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            file: None,
            service: None,
            field: None,
            message: message.into(),
            hint: None,
        }
    }

    pub fn at_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn at_service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    pub fn at_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut location = Vec::with_capacity(3);
        if let Some(file) = &self.file {
            location.push(file.clone());
        }
        if let Some(service) = &self.service {
            location.push(format!("service \"{service}\""));
        }
        write!(f, "{}", self.message)?;
        if !location.is_empty() {
            write!(f, " [{}]", location.join(" · "))?;
        }
        if let Some(field) = &self.field {
            write!(f, "\n     field: {field}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n     fix:   {hint}")?;
        }
        Ok(())
    }
}

/// 模块目录内 manifest 文件的规范相对路径（issue 定位用）。
pub fn manifest_file_of(dir: &str) -> String {
    format!("{dir}/project.manifest.toml")
}

/// 仅反序列化 project manifest（不做校验）——供收集式校验入口把"语法错误"
/// 与"语义问题"分开呈现，避免对已四段式渲染的校验文本二次包装。
pub fn parse_project_toml(content: &str) -> Result<ProjectManifest, ManifestError> {
    toml::from_str(content).map_err(|error| ManifestError::Parse(error.to_string()))
}

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
    let issues = collect_workspace_issues(manifest, "workspace.manifest.toml");
    issues
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.to_string()))
        .map_or(Ok(()), Err)
}

pub fn validate_project(manifest: &ProjectManifest) -> Result<(), ManifestError> {
    validate_project_at(manifest, "")
        .into_iter()
        .next()
        .map(|issue| ManifestError::Validation(issue.to_string()))
        .map_or(Ok(()), Err)
}

/// 在指定模块目录上下文中校验单个 project manifest，返回全部问题（不 fast-fail）。
///
/// `dir` 为模块目录名（issue 定位为 `<dir>/project.manifest.toml`）；
/// 传空串则不带文件定位（等价旧行为，供无目录上下文的调用方）。
pub fn validate_project_at(manifest: &ProjectManifest, dir: &str) -> Vec<ValidationIssue> {
    let file = if dir.is_empty() {
        None
    } else {
        Some(manifest_file_of(dir))
    };
    let locate = |issue: ValidationIssue| -> ValidationIssue {
        let issue = if let Some(file) = &file {
            issue.at_file(file.clone())
        } else {
            issue
        };
        issue.at_service(manifest.project.service_id.clone())
    };
    let mut issues = Vec::new();
    let project = &manifest.project;
    if manifest.schema_version != SCHEMA_VERSION {
        issues.push(locate(
            ValidationIssue::new(format!(
                "unsupported schema_version {}; expected {SCHEMA_VERSION}",
                manifest.schema_version
            ))
            .at_field("schema_version")
            .with_hint(format!(
                "set `schema_version = {SCHEMA_VERSION}` at the top of the file"
            )),
        ));
    }
    if !is_dns1123_label(&project.service_id) {
        issues.push(locate(
            ValidationIssue::new(format!(
                "project.service_id must be a DNS-1123 label (lowercase alphanumeric with '-', ≤63 chars): {}",
                project.service_id
            ))
            .at_field("project.service_id")
            .with_hint("example: \"backend-java\"; must be globally unique in this workspace"),
        ));
    }
    if project.name.trim().is_empty() {
        issues.push(
            locate(ValidationIssue::new("project.name must not be empty").at_field("project.name"))
                .with_hint("set a human-readable name, e.g. name = \"Java Backend B\""),
        );
    }
    if let Some(issue) = validate_argv_issue(&manifest.build.command, "build.command") {
        issues.push(locate(issue).at_field("build.command").with_hint(
            "argv array, e.g. [\"sh\", \"scripts/build.sh\"]; needs a shell? use [\"sh\", \"-c\", \"...\"]",
        ));
    }
    if let Some(issue) = validate_argv_issue(&manifest.run.command, "run.command") {
        issues.push(
            locate(issue).at_field("run.command").with_hint(
                "the service must listen on 0.0.0.0:$PORT ($PORT is injected per service)",
            ),
        );
    }
    if !manifest.run.migrate.is_empty()
        && let Some(issue) = validate_argv_issue(&manifest.run.migrate, "run.migrate")
    {
        issues.push(locate(issue).at_field("run.migrate"));
    }
    if let Some(issue) = relative_path_issue(&manifest.build.artifact, "build.artifact") {
        issues.push(
            locate(issue).at_field("build.artifact").with_hint(
                "path of the build output inside the project dir, e.g. \"artifact.zip\"",
            ),
        );
    }
    if manifest.run.shutdown_timeout_seconds == 0 {
        issues.push(
            locate(
                ValidationIssue::new("run.shutdown_timeout_seconds must be greater than zero")
                    .at_field("run.shutdown_timeout_seconds"),
            )
            .with_hint("remove the key to use the default 30, or set a positive value"),
        );
    }
    if project.kind == ProjectKind::Worker && manifest.proxy.is_some() {
        issues.push(
            locate(
                ValidationIssue::new("worker service must not declare [proxy]")
                    .at_field("proxy"),
            )
            .with_hint(
                "workers don't serve HTTP; remove the [proxy] section, or set kind = \"web\" if it does",
            ),
        );
    }
    if let Some(proxy) = &manifest.proxy
        && let Some(issue) = http_path_issue(&proxy.path, "proxy.path")
    {
        issues.push(
            locate(issue)
                .at_field("proxy.path")
                .with_hint("absolute URL path, unique per service, e.g. \"/api/java-b/\""),
        );
    }
    for (field, path) in [
        ("health.startup_path", &manifest.health.startup_path),
        ("health.readiness_path", &manifest.health.readiness_path),
        ("health.liveness_path", &manifest.health.liveness_path),
    ] {
        if let Some(issue) = http_path_issue(path, field) {
            issues.push(locate(issue).at_field(field));
        }
    }
    issues.extend(
        collect_log_issues(&manifest.logs.sources)
            .into_iter()
            .map(&locate),
    );
    for key in manifest.env.keys() {
        if is_reserved_env(key) {
            issues.push(
                locate(
                    ValidationIssue::new(format!(
                        "env key is reserved by the runtime: {key}"
                    ))
                    .at_field(format!("env.{key}")),
                )
                .with_hint(match key.as_str() {
                    "POSTGRES_USER" | "POSTGRES_PASSWORD" | "POSTGRES_DB" => format!(
                        "{key} is used by the built-in PostgreSQL initdb; the container DB password would silently diverge from your value. Use a different key and read the real credentials from the injected PG env"
                    ),
                    _ => format!(
                        "injected by the runtime; pick an application-specific key (e.g. \"MYAPP_{key}\")"
                    ),
                }),
            );
        }
    }
    issues
}

/// workspace 级 manifest 收集式校验。
pub fn collect_workspace_issues(manifest: &WorkspaceManifest, file: &str) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if manifest.schema_version != SCHEMA_VERSION {
        issues.push(
            ValidationIssue::new(format!(
                "unsupported schema_version {}; expected {SCHEMA_VERSION}",
                manifest.schema_version
            ))
            .at_file(file)
            .at_field("schema_version")
            .with_hint(format!(
                "set `schema_version = {SCHEMA_VERSION}` at the top"
            )),
        );
    }
    if manifest.workspace.name.trim().is_empty() {
        issues.push(
            ValidationIssue::new("workspace.name must not be empty")
                .at_file(file)
                .at_field("workspace.name")
                .with_hint("e.g. [workspace]\\nname = \"my-workspace\""),
        );
    }
    match manifest.pingap.mode {
        PingapMode::Managed if manifest.pingap.config.is_some() => issues.push(
            ValidationIssue::new("pingap.config is not allowed in managed mode")
                .at_file(file)
                .at_field("pingap.config")
                .with_hint("remove `config` (managed mode auto-generates routes from each [proxy]) or switch mode = \"extend\"/\"custom\""),
        ),
        PingapMode::Extend | PingapMode::Custom => {
            if let Some(issue) = relative_path_issue(
                manifest.pingap.config.as_deref().unwrap_or_default(),
                "pingap.config",
            ) {
                issues.push(issue.at_file(file).at_field("pingap.config").with_hint(
                    "directory of your own pingap config relative to the workspace root, e.g. \"pingap/\"",
                ));
            }
        }
        PingapMode::Managed => {}
    }
    issues
}

/// 校验跨模块使用的稳定服务标识（manifest、日志 selector、代理 upstream 同一规则）。
pub fn validate_service_id(value: &str) -> Result<(), ManifestError> {
    if is_dns1123_label(value) {
        Ok(())
    } else {
        Err(ManifestError::Validation(format!(
            "project.service_id must be a DNS-1123 label: {value}"
        )))
    }
}

/// 跨服务拓扑收集式校验：service_id 唯一性 / proxy 路由唯一性 / 依赖闭合与顺序。
///
/// 每条问题定位到声明方的 `<dir>/project.manifest.toml`；路由冲突类同时列出
/// 冲突双方，agent 可直接按提示改写。
pub fn collect_topology_issues(projects: &[DiscoveredProject]) -> Vec<ValidationIssue> {
    let enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    if enabled.is_empty() {
        return vec![ValidationIssue::new(
            "workspace has no enabled services (all projects disabled or none discovered)",
        )
        .at_file("workspace.manifest.toml")
        .with_hint(
            "check that each module dir has project.manifest.toml with [project] enabled = true (default)",
        )];
    }
    let mut issues = Vec::new();
    let mut ids: BTreeMap<String, &DiscoveredProject> = BTreeMap::new();
    let mut routes: BTreeMap<String, &DiscoveredProject> = BTreeMap::new();
    let mut catch_all_reported = false;
    for project in &enabled {
        let id = project.service_id();
        if let Some(previous) = ids.get(id) {
            issues.push(
                ValidationIssue::new(format!(
                    "duplicate service_id \"{id}\" declared by two modules: {} and {}",
                    previous.dir, project.dir
                ))
                .at_file(manifest_file_of(&project.dir))
                .at_service(id)
                .at_field("project.service_id")
                .with_hint(format!(
                    "service_id must be unique per workspace; rename one module's id, e.g. \"{id}-b\""
                )),
            );
        } else {
            ids.insert(id.to_owned(), project);
        }
        if let Some(proxy) = &project.manifest.proxy {
            if let Some(previous) = routes.get(proxy.path.as_str()) {
                issues.push(
                    ValidationIssue::new(format!(
                        "duplicate [proxy].path \"{}\" declared by services \"{}\" (dir {}) and \"{}\" (dir {})",
                        proxy.path,
                        previous.service_id(),
                        previous.dir,
                        project.service_id(),
                        project.dir
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("proxy.path")
                    .with_hint(format!(
                        "each web service needs a unique path prefix, e.g. \"{}\" in {} and \"{}\" in {}; pingap routes by path, so identical paths are ambiguous",
                        suggest_path(previous.service_id(), &proxy.path),
                        manifest_file_of(&previous.dir),
                        suggest_path(project.service_id(), &proxy.path),
                        manifest_file_of(&project.dir),
                    )),
                );
            } else {
                routes.insert(proxy.path.clone(), project);
            }
            // catch-all 冲突只报一次（在第二个声明方出现时）：按 catch_all_reported
            // 去重，N 个 "/" 服务不再产生 N 条几乎相同的报告。
            if proxy.path == "/"
                && !catch_all_reported
                && let Some(previous_catch_all) = enabled.iter().find(|other| {
                    other.service_id() != project.service_id()
                        && other
                            .manifest
                            .proxy
                            .as_ref()
                            .is_some_and(|proxy| proxy.path == "/")
                })
            {
                catch_all_reported = true;
                issues.push(
                    ValidationIssue::new(format!(
                        "multiple catch-all routes (proxy.path = \"/\"): \"{}\" (dir {}) and \"{}\" (dir {})",
                        previous_catch_all.service_id(),
                        previous_catch_all.dir,
                        project.service_id(),
                        project.dir
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_field("proxy.path")
                    .with_hint(
                        "only one service may own the root path; give the other a prefix like \"/app/\"",
                    ),
                );
            }
        }
    }
    for project in &enabled {
        for dependency in &project.manifest.run.depends_on {
            if dependency == project.service_id() {
                issues.push(
                    ValidationIssue::new(format!(
                        "service \"{}\" depends on itself",
                        project.service_id()
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("run.depends_on")
                    .with_hint("remove the self reference"),
                );
            } else if !ids.contains_key(dependency) {
                issues.push(
                    ValidationIssue::new(format!(
                        "service \"{}\" depends on missing or disabled service \"{dependency}\"",
                        project.service_id()
                    ))
                    .at_file(manifest_file_of(&project.dir))
                    .at_service(project.service_id())
                    .at_field("run.depends_on")
                    .with_hint(format!(
                        "check {dependency}: it may be disabled, missing, or have its own \
                         validation errors listed above (fix those first — do NOT remove this \
                         dependency to silence the error)"
                    )),
                );
            }
        }
    }
    issues.extend(cycle_issues(&enabled));
    issues
}

/// 兼容入口：fast-fail 版拓扑校验（取第一个 issue）。
pub fn validate_topology(projects: &[DiscoveredProject]) -> Result<Vec<String>, ManifestError> {
    let issues = collect_topology_issues(projects);
    if let Some(issue) = issues.into_iter().next() {
        return Err(ManifestError::Validation(issue.to_string()));
    }
    let enabled: Vec<_> = projects
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    // collect_topology_issues 已含环检测（cycle_issues），此处无环必成序——
    // topological_order 的 cycle Err 分支在此调用路径不可达，仅保留 Ok 语义。
    topological_order(&enabled)
}

/// 依赖环检测：报出环上的全部服务及各自声明文件。
fn cycle_issues(enabled: &[&DiscoveredProject]) -> Vec<ValidationIssue> {
    let dependencies: BTreeMap<String, BTreeSet<String>> = enabled
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
            let cycle_ids: Vec<&String> = remaining.keys().collect();
            let files: Vec<String> = enabled
                .iter()
                .filter(|project| remaining.contains_key(project.service_id()))
                .map(|project| manifest_file_of(&project.dir))
                .collect();
            return vec![
                ValidationIssue::new(format!(
                    "service dependency cycle detected among: {}",
                    cycle_ids
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .at_field("run.depends_on")
                .with_hint(format!(
                    "break the cycle in one of: {} (depends_on must form a DAG)",
                    files.join(", ")
                )),
            ];
        }
        for id in ready {
            remaining.remove(&id);
            result.push(id);
        }
    }
    Vec::new()
}

/// 路由冲突时的建议路径：按 service_id 生成唯一前缀（与冲突原值无关——
/// 冲突本身说明原值不可共用，建议值必须直接可抄）。
fn suggest_path(service_id: &str, _conflicting: &str) -> String {
    format!("/{service_id}/")
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

fn collect_log_issues(sources: &[LogSource]) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let mut ids = BTreeSet::new();
    for source in sources {
        if !is_dns1123_label(&source.id) {
            issues.push(
                ValidationIssue::new(format!(
                    "logs source id must be a DNS-1123 label: {}",
                    source.id
                ))
                .at_field(format!("logs.sources[id={}].id", source.id)),
            );
        }
        if !ids.insert(&source.id) {
            issues.push(
                ValidationIssue::new(format!("duplicate logs source id: {}", source.id))
                    .at_field("logs.sources.id"),
            );
        }
        if let Some(issue) = relative_path_issue(&source.glob, "logs.sources.glob") {
            issues.push(issue.at_field("logs.sources.glob"));
        }
        if source.glob.contains('/') || source.glob.contains('\\') {
            issues.push(
                ValidationIssue::new(format!(
                    "logs.sources.glob must be relative to the service log directory: {}",
                    source.glob
                ))
                .at_field("logs.sources.glob"),
            );
        }
        if source.format == LogFormat::Jsonl && source.multiline_start_pattern.is_some() {
            issues.push(
                ValidationIssue::new("multiline_start_pattern is only valid for text logs")
                    .at_field("logs.sources.multiline_start_pattern")
                    .with_hint("jsonl entries are single-line; remove the pattern or use format = \"text\""),
            );
        }
    }
    issues
}

/// 兼容旧 fast-fail 语义的 argv 校验（返回首条 issue）。
fn validate_argv_issue(argv: &[String], field: &str) -> Option<ValidationIssue> {
    (argv.is_empty() || argv.iter().any(|arg| arg.is_empty() || arg.contains('\0'))).then(|| {
        ValidationIssue::new(format!(
            "{field} must be a non-empty argv array without empty/NUL arguments"
        ))
    })
}

fn relative_path_issue(value: &str, field: &str) -> Option<ValidationIssue> {
    let path = Path::new(value);
    (value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }))
    .then(|| ValidationIssue::new(format!("{field} must be a safe relative path: {value}")))
}

fn http_path_issue(value: &str, field: &str) -> Option<ValidationIssue> {
    (!value.starts_with('/') || value.contains("..") || value.contains('?') || value.contains('#'))
        .then(|| {
            ValidationIssue::new(format!(
                "{field} must be an absolute URL path without traversal/query/fragment: {value}"
            ))
        })
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

/// 校验 [env] 不得覆盖运行时保留键。
///
/// POSTGRES_USER/POSTGRES_PASSWORD/POSTGRES_DB 保留原因：PG initdb 使用容器 env 的
/// POSTGRES_PASSWORD 设置密码（docker/app-runtime-base/pg-supervisor-entry.sh），
/// 用户在 [env] 覆盖会导致服务与 PG 实际密码静默错开、连不上库。
///
/// PGHOST/PGPORT 明确不保留：用户代码连外部 PostgreSQL 是合法场景（只影响其服务
/// 进程自身连接），且 app-cli supervisor 的 wait_for_pg 读的是 app-cli 自身进程 env，
/// workspace [env] 只注入服务子进程，互不影响。
fn is_reserved_env(key: &str) -> bool {
    matches!(
        key,
        "PORT"
            | "HOST"
            | "HOSTNAME"
            | "APP_LOG_DIR"
            | "APP_SERVICE_ID"
            | "APP_RELEASE_ID"
            | "POSTGRES_USER"
            | "POSTGRES_PASSWORD"
            | "POSTGRES_DB"
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

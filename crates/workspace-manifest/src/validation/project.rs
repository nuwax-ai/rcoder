//! 单 project / workspace 收集式校验 + 字段级 issue 构造器。

use std::path::Path;

use crate::{
    ManifestError, PingapMode, ProjectKind, ProjectManifest, SCHEMA_VERSION, WorkspaceManifest,
};

use super::issue::{ValidationIssue, manifest_file_of};
use super::topology::collect_log_issues;

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
    if let Some(devbuild) = &manifest.devbuild
        && let Some(issue) = validate_argv_issue(&devbuild.command, "devbuild.command")
    {
        issues.push(
            locate(issue).at_field("devbuild.command").with_hint(
                "dev 阶段编译/检查命令（仅源码态 dev 链路生效，缺省回落 [build].command）",
            ),
        );
    }
    if let Some(issue) = validate_argv_issue(&manifest.run.command, "run.command") {
        issues.push(
            locate(issue).at_field("run.command").with_hint(
                "the service must listen on 0.0.0.0:$PORT ($PORT is injected per service)",
            ),
        );
    }
    if let Some(devrun) = &manifest.devrun
        && let Some(issue) = validate_argv_issue(&devrun.command, "devrun.command")
    {
        issues.push(locate(issue).at_field("devrun.command").with_hint(
            "dev 阶段热加载启动命令（配置即切源码态，缺省回落 [run].command）；\
             需监听 0.0.0.0:$PORT（与 run.command 同款注入）",
        ));
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

/// 兼容旧 fast-fail 语义的 argv 校验（返回首条 issue）。
pub(super) fn validate_argv_issue(argv: &[String], field: &str) -> Option<ValidationIssue> {
    (argv.is_empty() || argv.iter().any(|arg| arg.is_empty() || arg.contains('\0'))).then(|| {
        ValidationIssue::new(format!(
            "{field} must be a non-empty argv array without empty/NUL arguments"
        ))
    })
}

pub(super) fn relative_path_issue(value: &str, field: &str) -> Option<ValidationIssue> {
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

pub(super) fn http_path_issue(value: &str, field: &str) -> Option<ValidationIssue> {
    (!value.starts_with('/') || value.contains("..") || value.contains('?') || value.contains('#'))
        .then(|| {
            ValidationIssue::new(format!(
                "{field} must be an absolute URL path without traversal/query/fragment: {value}"
            ))
        })
}

pub(super) fn is_dns1123_label(value: &str) -> bool {
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
pub(super) fn is_reserved_env(key: &str) -> bool {
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

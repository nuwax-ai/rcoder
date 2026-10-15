//! Safe, non-executing project detection and draft confirmation.

use std::path::Path;

use crate::models::DetectionResult;
use file_server::error::{AppError, AppResult};

pub async fn detect_project(workspace: &Path, project_dir: &str) -> AppResult<DetectionResult> {
    validate_project_dir(project_dir)?;
    let project = file_server::path_safety::ensure_within(workspace, project_dir)
        .map_err(|_| AppError::validation("project_dir escapes workspace"))?;
    // 全段走 tokio::fs：本函数每次导入请求要 stat 九次（自身 2 + detect_type 6 +
    // suggestion 1），阻塞的 Path 方法会占住 tokio worker。metadata 跟随符号链接、
    // 遇错按 false，与 Path::is_dir()/is_file() 语义一致。
    if !tokio::fs::metadata(&project)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(AppError::resource(format!(
            "import project directory not found: {project_dir}"
        )));
    }
    if tokio::fs::try_exists(project.join("project.manifest.toml"))
        .await
        .unwrap_or(false)
    {
        return Err(AppError::business(format!(
            "project already has a confirmed manifest: {project_dir}"
        )));
    }
    let service_id = normalize_service_id(project_dir)?;
    let detected = detect_type(&project).await?;
    let (build, run, health, log_format, warnings) = suggestion(&project, detected).await;
    let manifest = format!(
        r#"schema_version = 1

[project]
service_id = "{service_id}"
name = "{service_id}"
type = "{detected}"
kind = "web"
enabled = true

[build]
command = {build}
artifact = "artifact.zip"

[run]
command = {run}
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[health]
startup_path = "{health}"
readiness_path = "{health}"
liveness_path = "{health}"

[[logs.sources]]
id = "application"
glob = "application*.log"
format = "{log_format}"
"#
    );
    let draft = project.join("project.manifest.draft.toml");
    tokio::fs::write(&draft, &manifest)
        .await
        .map_err(|error| AppError::file(format!("write project manifest draft: {error}")))?;
    Ok(DetectionResult {
        project_dir: project_dir.to_owned(),
        detected_type: detected.to_owned(),
        draft_path: format!("{project_dir}/project.manifest.draft.toml"),
        manifest,
        warnings,
    })
}

pub async fn confirm_project(workspace: &Path, project_dir: &str) -> AppResult<String> {
    validate_project_dir(project_dir)?;
    let project = file_server::path_safety::ensure_within(workspace, project_dir)
        .map_err(|_| AppError::validation("project_dir escapes workspace"))?;
    let draft = project.join("project.manifest.draft.toml");
    let content = tokio::fs::read_to_string(&draft)
        .await
        .map_err(|error| AppError::resource(format!("read project manifest draft: {error}")))?;
    shared_types::parse_project(&content).map_err(|error| {
        AppError::validation(format!("invalid project manifest draft: {error}"))
    })?;
    let confirmed = project.join("project.manifest.toml");
    // try_exists 而非阻塞的 Path::exists()；这条检查不能省，它挡住覆盖已确认的 manifest
    if tokio::fs::try_exists(&confirmed).await.unwrap_or(false) {
        return Err(AppError::business(format!(
            "confirmed project manifest already exists: {project_dir}"
        )));
    }
    tokio::fs::rename(&draft, &confirmed)
        .await
        .map_err(|error| AppError::file(format!("confirm project manifest: {error}")))?;
    Ok(format!("{project_dir}/project.manifest.toml"))
}

async fn detect_type(project: &Path) -> AppResult<&'static str> {
    let candidates = [
        ("package.json", "node"),
        ("pom.xml", "java"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("requirements.txt", "python"),
        ("Cargo.toml", "rust"),
    ];
    // 用 for 循环而非 iterator filter：filter 闭包是同步的、内部无法 await。
    // 保持 candidates 原序 —— 下面 ambiguous 分支要按该顺序列出命中的 marker。
    let mut detected: Vec<(&str, &str)> = Vec::new();
    for (file, project_type) in candidates {
        if tokio::fs::metadata(project.join(file))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            detected.push((file, project_type));
        }
    }
    if detected.is_empty() {
        return Err(AppError::business(
            "unable to detect project type from supported dependency files",
        ));
    }
    let types: std::collections::BTreeSet<_> = detected
        .iter()
        .map(|(_, project_type)| *project_type)
        .collect();
    if types.len() != 1 {
        return Err(AppError::business(format!(
            "ambiguous project type; detected markers: {}",
            detected
                .iter()
                .map(|(file, _)| *file)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    types
        .first()
        .copied()
        .ok_or_else(|| AppError::business("detected project type disappeared"))
}

async fn suggestion(
    project: &Path,
    project_type: &str,
) -> (String, String, &'static str, &'static str, Vec<String>) {
    let script_exists = tokio::fs::metadata(project.join("scripts/build-standalone.sh"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false);
    let build = r#"["sh", "scripts/build-standalone.sh"]"#.into();
    let (run, health, format) = match project_type {
        "java" => (r#"["java", "-jar", "app.jar"]"#, "/actuator/health", "text"),
        "go" | "rust" => (r#"["./server"]"#, "/health", "jsonl"),
        "python" => (r#"["python3", "main.py"]"#, "/health", "text"),
        _ => (r#"["node", "server.js"]"#, "/health", "jsonl"),
    };
    let mut warnings = vec![
        "Confirm the build artifact name and that the zip root contains all runtime files.".into(),
        "Confirm run.command, health paths, proxy route and log glob before confirming.".into(),
    ];
    if !script_exists {
        warnings.push(
            "scripts/build-standalone.sh is missing; create it before confirming this draft."
                .into(),
        );
    }
    (build, run.into(), health, format, warnings)
}

fn normalize_service_id(project_dir: &str) -> AppResult<String> {
    let name = Path::new(project_dir)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::validation("project_dir must have a UTF-8 directory name"))?;
    let normalized = name
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if normalized.is_empty() || normalized.len() > 63 {
        return Err(AppError::validation(
            "cannot derive a DNS-1123 service_id from project_dir",
        ));
    }
    Ok(normalized)
}

fn validate_project_dir(project_dir: &str) -> AppResult<()> {
    let mut components = Path::new(project_dir).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(AppError::validation(
            "project_dir must name exactly one workspace-level child directory",
        ));
    }
    Ok(())
}

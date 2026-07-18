//! computer 执行类 handlers: execute-command / install-project / get-logs /
//! build-agent-package / cleanup-build-artifacts + 命令执行/产物解析辅助。

use std::path::{Path, PathBuf};

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::pnpm::{self, InstallOptions};
use crate::service::pnpm_config;

use super::process_capture::{capture_command, run_capture};
use super::{resolve_computer_target, ws_path};

// ── execute-command ─────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecCommandBody {
    user_id: String,
    c_id: String,
    command: String,
}

/// `POST /api/computer/execute-command` (对齐 nuwax executeCommand; shell 执行 + 超时 + 捕获输出)。
/// command 是 agent 提供的 shell 命令串, 故经 shell -c (与 nuwax child_process.exec 一致)。
/// shell 优先用 `BASH_PATH` (未配置则 sh); stdout/stderr 截断到 50MB (对齐 nuwax maxBuffer)。
#[utoipa::path(post, path = "/execute-command", request_body = ExecCommandBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<ExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    let cwd = ws_path(&state, &body.user_id, &body.c_id).await?;
    if !cwd.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    if body.command.trim().is_empty() {
        return Err(AppError::validation("command cannot be empty"));
    }
    let timeout_secs = state.config.dev_command_timeout_secs;
    // shell: BASH_PATH 非空则用之, 否则 sh (对齐 nuwax execOptions.shell)
    let shell = {
        let b = state.config.bash_path.trim();
        if b.is_empty() { "sh" } else { b }
    };
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c").arg(&body.command);
    cmd.current_dir(&cwd);
    cmd.env("NODE_ENV", "development");
    cmd.env_remove("CI");
    cmd.env_remove("NPM_CONFIG_PRODUCTION");
    let result = capture_command(&mut cmd, "execute-command", timeout_secs).await?;
    Ok(Json(json!({
        // TS 外层响应始终 success=true，命令结果由 exitCode 表示。
        "success": true,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "exitCode": result.exit_code,
    })))
}

// ── get-logs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetLogsQuery {
    user_id: String,
    c_id: String,
    #[serde(default = "default_tail_lines")]
    tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

/// `GET /api/computer/get-logs` (对齐 nuwax getLatestLogs; 读 .logs/ 下 mtime 最新的文件末尾 N 行)。
/// 空场景区分 message, logFileName=null; 成功 message="Get log successfully"; 过滤空行。
#[utoipa::path(
    get,
    path = "/get-logs",
    params(GetLogsQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<GetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    let log_dir = resolve_computer_target(&state, &q.user_id, &q.c_id, None).await?.join(".logs");
    let empty_resp = |msg: &str| {
        Json(json!({
            "success": true,
            "message": msg,
            "logs": [],
            "totalLines": 0,
            "startIndex": 1,
            "logFileName": null,
        }))
    };
    if !log_dir.exists() {
        return Ok(empty_resp("Log directory does not exist"));
    }
    let latest = match latest_log_file(&log_dir).await? {
        Some(p) => p,
        None => return Ok(empty_resp("No log file found")),
    };
    let name = latest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("log")
        .to_string();
    let content = crate::service::fs_util::read_to_string_bounded(
        &latest,
        state.config.log_read_max_bytes,
        "computer log",
    )
    .await?;
    // 过滤空行 (对齐 nuwax filter(l => l.length > 0))
    let all: Vec<&str> = content.split('\n').filter(|l| !l.is_empty()).collect();
    let total = all.len();
    let start = total.saturating_sub(q.tail_lines);
    let logs: Vec<Value> = all[start..]
        .iter()
        .enumerate()
        .map(|(i, l)| json!({ "line": start + i + 1, "content": l }))
        .collect();
    Ok(Json(json!({
        "success": true,
        "message": "Get log successfully",
        "logs": logs,
        "totalLines": total,
        "startIndex": start + 1,
        "logFileName": name,
    })))
}

/// 读目录下 mtime 最新的文件路径 (无文件返回 None)。
async fn latest_log_file(dir: &Path) -> AppResult<Option<PathBuf>> {
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| AppError::system(format!("read log dir: {e}")))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| AppError::system(format!("read log entry: {e}")))?
    {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
                latest = Some((mtime, entry.path()));
            }
        }
    }
    Ok(latest.map(|(_, p)| p))
}

// ── install-project ─────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallBody {
    user_id: String,
    c_id: String,
    programming_language: String,
}

/// `POST /api/computer/install-project` (对齐 nuwax installProjectDependencies)。
/// typescript → 递归找 package.json 目录 pnpm install; python → 找 requirements/pyproject pip install。
#[utoipa::path(post, path = "/install-project", request_body = InstallBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn install_project(
    State(state): State<AppState>,
    Json(body): Json<InstallBody>,
) -> Result<Json<Value>, AppError> {
    let ws = ws_path(&state, &body.user_id, &body.c_id).await?;
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let lang = body.programming_language.to_ascii_lowercase();
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let (program, args, project_dir): (&str, Vec<&str>, Option<PathBuf>) = match lang.as_str() {
        "typescript" | "ts" => {
            // projectDir = findPackageScript || findNodeProjectDir (对齐 nuwax)
            let dir = match find_package_script(&ws, &skip).await {
                Some(d) => Some(d),
                None => find_first(&ws, "package.json").await,
            };
            ("pnpm", Vec::new(), dir)
        }
        "python" | "py" => {
            // 优先 pyproject.toml (pip install -e .), 否则 requirements.txt
            let (dir, args) = if let Some(d) = find_first(&ws, "pyproject.toml").await {
                (Some(d), vec!["install", "-e", "."])
            } else {
                (
                    find_first(&ws, "requirements.txt").await,
                    vec!["install", "-r", "requirements.txt"],
                )
            };
            ("pip", args, dir)
        }
        other => {
            return Err(AppError::validation(format!(
                "unsupported programmingLanguage: {other}"
            )));
        }
    };
    let project_dir = project_dir.ok_or_else(|| {
        AppError::business(
            "project manifest (package.json / pyproject.toml / requirements.txt) not found",
        )
    })?;
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install 前准备 .npmrc (package-import-method=copy + built-deps sanitize + 3 行),
    // best-effort (失败仅 warn, 不阻断 install; 对齐 nuwax ensurePnpmInstallConfig)
    if program == "pnpm" {
        pnpm_config::ensure_pnpm_install_config(&project_dir).await;
    }
    if program == "pnpm" {
        let options = InstallOptions {
            prefer_offline: true,
            extra_args: vec![
                "--config.production=false".to_string(),
                "--config.confirmModulesPurge=false".to_string(),
                "--config.dangerouslyAllowAllBuilds=true".to_string(),
            ],
        };
        pnpm::install(&project_dir, &options, None, timeout)
            .await
            .map_err(|error| {
                AppError::system(format!("Project dependencies install failed: {error}"))
            })?;
    } else {
        let (stdout, stderr, code) = run_capture(program, &args, &project_dir, timeout).await?;
        if code != 0 {
            let detail = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Err(AppError::system(format!(
                "Project dependencies install failed: {detail}"
            )));
        }
    }
    Ok(Json(json!({
        "success": true,
        "message": "Project dependencies installed successfully",
        "projectDir": project_dir.display().to_string(),
        "programmingLanguage": lang,
    })))
}

// ── build-agent-package ─────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildAgentBody {
    user_id: String,
    c_id: String,
    agent_id: String,
    version: String,
}

/// `POST /api/computer/build-agent-package` (对齐 nuwax buildAgentPackage)。
/// 递归找含 scripts/package-platforms.mjs 的目录 → pnpm install →
/// `node scripts/package-platforms.mjs agent-{id} {ver} {dir}/dist-packages --print-artifacts`
/// → 解析 stdout 中产物 (path 转 workspace 相对, platform 从文件名提取)。响应无 stdout。
#[utoipa::path(post, path = "/build-agent-package", request_body = BuildAgentBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn build_agent_package(
    State(state): State<AppState>,
    Json(body): Json<BuildAgentBody>,
) -> Result<Json<Value>, AppError> {
    let ws = ws_path(&state, &body.user_id, &body.c_id).await?;
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    // 递归找 scripts/package-platforms.mjs 所在目录 (对齐 nuwax findPackageScript;
    // skip ZIP_WORKSPACE_EXCLUDE ∪ {dist-packages}, 而非仅 package.json)
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let pkg_dir = find_package_script(&ws, &skip)
        .await
        .ok_or_else(|| AppError::business("package-platforms.mjs not found in workspace"))?;
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install 前准备 .npmrc (best-effort, 对齐 nuwax runPnpmInstall → ensurePnpmInstallConfig)
    pnpm_config::ensure_pnpm_install_config(&pkg_dir).await;
    // pnpm install (含 devDependencies; esbuild/typescript 在 devDependencies 中)
    pnpm::install(&pkg_dir, &InstallOptions::default(), None, timeout)
        .await
        .map_err(|error| AppError::system(format!("pnpm install failed: {error}")))?;
    // 打包
    let dist_packages = pkg_dir.join("dist-packages");
    let agent_name = format!("agent-{}", body.agent_id);
    let (stdout, stderr, code) = run_capture(
        "node",
        &[
            "scripts/package-platforms.mjs",
            &agent_name,
            &body.version,
            &dist_packages.to_string_lossy(),
            "--print-artifacts",
        ],
        &pkg_dir,
        timeout,
    )
    .await?;
    if code != 0 {
        return Err(AppError::system(format!(
            "package-platforms.mjs failed (exit {code}): {stderr}"
        )));
    }
    // 解析产物 (path 转 workspace 相对, platform 从文件名提取; 无 stdout 字段)
    let artifacts = parse_artifacts(&stdout, &ws);
    Ok(Json(json!({ "success": true, "artifacts": artifacts })))
}

// ── cleanup-build-artifacts ─────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CleanupBuildArtifactsBody {
    user_id: String,
    c_id: String,
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/cleanup-build-artifacts` (对齐 nuwax cleanupBuildArtifacts; 删 dist-packages)。
/// 返回 {success, cleaned} (字段 cleaned, 非 removed; 无 message)。
/// 递归找 scripts/package-platforms.mjs 所在 projectDir, 删其 dist-packages (对齐 nuwax)。
#[utoipa::path(post, path = "/cleanup-build-artifacts", request_body = CleanupBuildArtifactsBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn cleanup_build_artifacts(
    State(state): State<AppState>,
    Json(body): Json<CleanupBuildArtifactsBody>,
) -> Result<Json<Value>, AppError> {
    let ws = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    )
    .await?;
    if !ws.exists() {
        return Ok(Json(json!({ "success": true, "cleaned": false })));
    }
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let project_dir = match find_package_script(&ws, &skip).await {
        Some(d) => d,
        None => return Ok(Json(json!({ "success": true, "cleaned": false }))),
    };
    let dist = project_dir.join("dist-packages");
    let cleaned = if dist.exists() {
        match tokio::fs::remove_dir_all(&dist).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "cleanup dist-packages failed");
                false
            }
        }
    } else {
        false
    };
    Ok(Json(json!({ "success": true, "cleaned": cleaned })))
}

// ── 命令执行 / 产物解析辅助 ──────────────────────────────────────────────────────

/// 递归找含 `manifest` 的最近目录, 返回该目录 (BFS)。
async fn find_first(root: &Path, manifest: &str) -> Option<PathBuf> {
    use std::collections::VecDeque;
    let mut q = VecDeque::new();
    q.push_back(root.to_path_buf());
    while let Some(dir) = q.pop_front() {
        if dir.join(manifest).exists() {
            return Some(dir);
        }
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name();
                // 跳过常见大目录
                if matches!(
                    name.to_str(),
                    Some("node_modules" | ".git" | "dist" | ".pnpm-store")
                ) {
                    continue;
                }
                q.push_back(entry.path());
            }
        }
    }
    None
}

/// build-agent-package / cleanup / install 定位项目目录时的搜索跳过集合
/// (对齐 nuwax PACKAGE_SEARCH_SKIP_DIRS = ZIP_WORKSPACE_EXCLUDE ∪ {dist-packages})。
fn package_search_skip_dirs(zip_workspace_exclude: &[String]) -> Vec<String> {
    let mut v = zip_workspace_exclude.to_vec();
    if !v.iter().any(|d| d == "dist-packages") {
        v.push("dist-packages".to_string());
    }
    v
}

/// 递归查找含 `scripts/package-platforms.mjs` 的目录 (对齐 nuwax findPackageScript)。
/// 深度优先, 跳过 skip_dirs 命中的目录名。
async fn find_package_script(root: &Path, skip_dirs: &[String]) -> Option<PathBuf> {
    if root.join("scripts").join("package-platforms.mjs").exists() {
        return Some(root.to_path_buf());
    }
    let Ok(mut rd) = tokio::fs::read_dir(root).await else {
        return None;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if skip_dirs.iter().any(|d| d == &name) {
            continue;
        }
        if let Some(found) = Box::pin(find_package_script(&entry.path(), skip_dirs)).await {
            return Some(found);
        }
    }
    None
}

/// 从 package-platforms stdout 解析产物列表: {path (workspace 相对), fileName, platform}。
/// path 对齐 nuwax: 相对 workspace 目录, 路径分隔符转 `/`。
fn parse_artifacts(stdout: &str, workspace: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if !(t.ends_with(".tar.gz")
            || t.ends_with(".tar.bz2")
            || t.ends_with(".zip")
            || t.ends_with(".tgz"))
        {
            continue;
        }
        // t 可能是相对路径或绝对路径; 统一转 workspace 相对
        let abs = if Path::new(t).is_absolute() {
            PathBuf::from(t)
        } else {
            workspace.join(t)
        };
        let rel = abs
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| t.to_string());
        let file_name = t.rsplit('/').next().unwrap_or(t).to_string();
        let platform = extract_platform_from_filename(&file_name).unwrap_or_default();
        out.push(json!({
            "path": rel,
            "fileName": file_name,
            "platform": platform,
        }));
    }
    out
}

/// 从产物文件名提取 platform (对齐 nuwax extractPlatformFromFileName):
/// 去掉 .tar.gz/.tar.bz2/.tgz/.zip 后缀, 按 `-` 分割, 找最后一个形如 x.y.z 的版本段,
/// platform = parts[2..versionIdx] (跳过 agent-{id})。
fn extract_platform_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name
        .strip_suffix(".tar.gz")
        .or_else(|| file_name.strip_suffix(".tar.bz2"))
        .or_else(|| file_name.strip_suffix(".tgz"))
        .or_else(|| file_name.strip_suffix(".zip"))
        .unwrap_or(file_name);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    // 找最后一个完整 semver 版本段。版本语义与 package.json 检测共用 node-semver，
    // 避免在这里再维护一套按 `.` 分段的数字解析。
    let mut version_idx = None;
    for (i, p) in parts.iter().enumerate().rev() {
        if node_semver::Version::parse(p).is_ok() {
            version_idx = Some(i);
            break;
        }
    }
    let vidx = version_idx?;
    if vidx <= 2 {
        return None;
    }
    // platform = parts[2..vidx]
    let platform = parts[2..vidx].join("-");
    if platform.is_empty() {
        None
    } else {
        Some(platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_platform_handles_multi_segment() {
        // agent-{id}-{platform}-{ver}.{ext} → platform 可能多段
        assert_eq!(
            extract_platform_from_filename("agent-foo-linux-x64-1.0.0.zip"),
            Some("linux-x64".to_string())
        );
        assert_eq!(
            extract_platform_from_filename("agent-foo-darwin-2.1.0.tar.gz"),
            Some("darwin".to_string())
        );
        assert_eq!(
            extract_platform_from_filename("agent-bar-win32-x64-0.9.1.tgz"),
            Some("win32-x64".to_string())
        );
    }

    #[test]
    fn extract_platform_returns_none_when_no_version() {
        assert_eq!(
            extract_platform_from_filename("agent-foo-linux-x64.zip"),
            None
        );
        assert_eq!(extract_platform_from_filename("nope.zip"), None);
    }
}

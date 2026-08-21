//! computer 包管理 handlers: install-project / build-agent-package / cleanup-build-artifacts。
//!
//! 从 [`super::exec`] 拆出 (执行/日志类留在 exec)。包搜索 + 产物解析在
//! [`crate::service::package_build`]。

use std::path::PathBuf;

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::process_capture::run_capture;
use super::{resolve_computer_target, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::service::package_build;
use crate::service::pnpm::{self, InstallOptions};
use crate::service::pnpm_config;

// ── install-project ─────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
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
    install_project_impl(&state, ws, &body.programming_language).await
}

/// install-project 的 workspace 无关实现 (typescript→pnpm / python→pip, 按语言找 manifest)。
pub(crate) async fn install_project_impl(
    state: &AppState,
    ws: PathBuf,
    programming_language: &str,
) -> Result<Json<Value>, AppError> {
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let lang = programming_language.to_ascii_lowercase();
    let skip = package_build::package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let (program, args, project_dir): (&str, Vec<&str>, Option<PathBuf>) = match lang.as_str() {
        "typescript" | "ts" => {
            // projectDir = findPackageScript || findNodeProjectDir (对齐 nuwax)
            let dir = match package_build::find_package_script(&ws, &skip).await {
                Some(d) => Some(d),
                None => package_build::find_first(&ws, "package.json").await,
            };
            ("pnpm", Vec::new(), dir)
        }
        "python" | "py" => {
            // 优先 pyproject.toml (pip install -e .), 否则 requirements.txt
            let (dir, args) =
                if let Some(d) = package_build::find_first(&ws, "pyproject.toml").await {
                    (Some(d), vec!["install", "-e", "."])
                } else {
                    (
                        package_build::find_first(&ws, "requirements.txt").await,
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
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
    // agentId 同 user_id/c_id: TS 原版 buildAgentPackage 标注 {string|number},Java 后端传 DB bigint(整数)。
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
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
    let skip = package_build::package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let pkg_dir = package_build::find_package_script(&ws, &skip)
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
    let artifacts = package_build::parse_artifacts(&stdout, &ws);
    Ok(Json(json!({ "success": true, "artifacts": artifacts })))
}

// ── cleanup-build-artifacts ─────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CleanupBuildArtifactsBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
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
    let skip = package_build::package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let project_dir = match package_build::find_package_script(&ws, &skip).await {
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

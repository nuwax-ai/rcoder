//! computer 包管理 handlers: install-project / build-agent-package / cleanup-build-artifacts。
//!
//! 从 [`super::exec`] 拆出 (执行/日志类留在 exec)。包搜索 + 产物解析在
//! [`crate::service::package_build`]。

use axum::extract::State;

use super::{ServiceScope, resolve_computer_target, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{
    BuildAgentBody, BuildAgentPackageResult, CleanupBuildArtifactsBody,
    CleanupBuildArtifactsResult, InstallBody, InstallProjectResult,
};
use crate::ops::packages::install_project_impl;
use crate::ops::process_capture::run_capture;
use crate::service::package_build;
use crate::service::pnpm::{self, InstallOptions};
use crate::service::pnpm_config;

// ── install-project ─────────────────────────────────────────────────────────────

/// 安装项目依赖
///
/// 对齐 nuwax installProjectDependencies。
/// typescript → 递归找 package.json 目录 pnpm install; python → 找 requirements/pyproject pip install。
#[utoipa::path(post, path = "/install-project", request_body = InstallBody, responses((status = 200, description = "依赖安装结果", body = InstallProjectResult), crate::openapi::ErrorApiResponses), tag = "Computer")]
pub(crate) async fn install_project(
    State(state): State<AppState>,
    Json(body): Json<InstallBody>,
) -> Result<Json<InstallProjectResult>, AppError> {
    let ws = ws_path(
        &state,
        &body.user_id,
        &body.c_id,
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    install_project_impl(&state, ws, &body.programming_language).await
}

// ── build-agent-package ─────────────────────────────────────────────────────────

/// 构建 agent 安装包
///
/// 对齐 nuwax buildAgentPackage。
/// 递归找含 scripts/package-platforms.mjs 的目录 → pnpm install →
/// `node scripts/package-platforms.mjs agent-{id} {ver} {dir}/dist-packages --print-artifacts`
/// → 解析 stdout 中产物 (path 转 workspace 相对, platform 从文件名提取)。响应无 stdout。
#[utoipa::path(post, path = "/build-agent-package", request_body = BuildAgentBody, responses((status = 200, description = "打包产物列表", body = BuildAgentPackageResult), crate::openapi::ErrorApiResponses), tag = "Computer")]
pub(crate) async fn build_agent_package(
    State(state): State<AppState>,
    Json(body): Json<BuildAgentBody>,
) -> Result<Json<BuildAgentPackageResult>, AppError> {
    let ws = ws_path(
        &state,
        &body.user_id,
        &body.c_id,
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    if !tokio::fs::try_exists(&ws).await.unwrap_or(false) {
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
    let artifacts = package_build::parse_artifacts(&stdout, &pkg_dir, &ws);
    Ok(Json(BuildAgentPackageResult {
        success: true,
        artifacts,
    }))
}

// ── cleanup-build-artifacts ─────────────────────────────────────────────────────

/// 清理构建产物
///
/// 对齐 nuwax cleanupBuildArtifacts; 删 dist-packages。
/// 返回 {success, cleaned} (字段 cleaned, 非 removed; 无 message)。
/// 递归找 scripts/package-platforms.mjs 所在 projectDir, 删其 dist-packages (对齐 nuwax)。
#[utoipa::path(post, path = "/cleanup-build-artifacts", request_body = CleanupBuildArtifactsBody, responses((status = 200, description = "清理结果", body = CleanupBuildArtifactsResult), crate::openapi::ErrorApiResponses), tag = "Computer")]
pub(crate) async fn cleanup_build_artifacts(
    State(state): State<AppState>,
    Json(body): Json<CleanupBuildArtifactsBody>,
) -> Result<Json<CleanupBuildArtifactsResult>, AppError> {
    let ws = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    if !tokio::fs::try_exists(&ws).await.unwrap_or(false) {
        return Ok(Json(CleanupBuildArtifactsResult {
            success: true,
            cleaned: false,
        }));
    }
    let skip = package_build::package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let project_dir = match package_build::find_package_script(&ws, &skip).await {
        Some(d) => d,
        None => {
            return Ok(Json(CleanupBuildArtifactsResult {
                success: true,
                cleaned: false,
            }));
        }
    };
    let dist = project_dir.join("dist-packages");
    let cleaned = if tokio::fs::try_exists(&dist).await.unwrap_or(false) {
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
    Ok(Json(CleanupBuildArtifactsResult {
        success: true,
        cleaned,
    }))
}

//! UserApp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - `app_id` = file-server `project_id`（复用 [`WorkspaceResolver::resolve_project`]，
//!   不新建 resolve_userapp）。workspace 根下有多个子项目（前端/后端/...）。
//! - file-server 严格读取 Manifest v1，并自动发现一级子项目。
//! - 组装成版本化整体包 `workspace-package-<release_id>.zip`，内含 release lock。
//!
//! 子模块：
//! - [`manifest`]：两级 manifest 类型 + 解析
//! - [`assemble`]：整体包 zip 组装（raw copy 子产物 + 入口文件 + pingap 配置写入）
//! - [`pingap`]：pingap 反代配置（`pingap.toml`）+ `.service-ports` 生成（独立可扩展）

mod assemble;
pub mod import;
mod manifest;

// 重导出 manifest 类型：保持 userapp 模块公开面。
pub use manifest::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};

use std::path::PathBuf;

use crate::error::{AppError, AppResult};
use crate::service::build_generic::build_generic;
use crate::service::build_manager::BuildManager;
use crate::workspace::{ProjectContext, WorkspaceResolver};

use assemble::assemble_workspace_package;
use manifest::{ReleaseMetadata, build_release_lock, read_workspace_manifest};

/// 整体包产物文件名（放在 workspace 根，供 `GET /api/userapp/static` 下载）。
pub const WORKSPACE_PACKAGE_PREFIX: &str = "workspace-package-";

#[derive(Debug, Clone)]
pub struct WorkspaceBuildArtifact {
    pub release_id: String,
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// 一个已构建完成的子项目（path + 产物绝对路径），供组装阶段使用。
#[derive(Clone)]
struct BuiltProject {
    path: String,
    artifact: PathBuf,
}

/// workspace 多项目打包主流程。
///
/// 1. `resolve_project(app_id)` → workspace 根（app_id = project_id）
/// 2. 读 `workspace.manifest.toml` → 子项目列表
/// 3. 遍历子项目：读 `project.manifest.toml` → `build_generic(cmd, artifact, cwd={ws}/{path})`
/// 4. [`assemble::assemble_workspace_package`] 组装整体包（含 pingap 配置 + `.service-ports`）
///
/// 返回版本化整体包及其 release ID、摘要和大小。
pub async fn build_workspace_package(
    resolver: &dyn WorkspaceResolver,
    build_manager: &BuildManager,
    app_id: &str,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    timeout_secs: u64,
) -> AppResult<WorkspaceBuildArtifact> {
    // 1. workspace 根（app_id 复用 project_id）
    let ws = resolver
        .resolve_project(&ProjectContext {
            project_id: app_id.to_string(),
            tenant_id: tenant_id.map(str::to_string),
            space_id: space_id.map(str::to_string),
            isolation_type: None,
        })
        .await?;
    if !ws.is_dir() {
        return Err(AppError::resource(format!(
            "UserApp workspace not found: {} (app_id={app_id})",
            ws.display()
        )));
    }

    // 2. 严格解析 workspace Manifest v1。
    let manifest = read_workspace_manifest(&ws).await?;

    // 3. 自动发现子项目（扫描含 project.manifest.toml 的一级子目录）
    let discovered = manifest::discover_projects(&ws)
        .map_err(|e| AppError::system(format!("discover projects in {}: {e}", ws.display())))?;
    if discovered.is_empty() {
        return Err(AppError::business(format!(
            "no sub-projects found (no project.manifest.toml in any subdirectory of workspace=\"{}\")",
            manifest.workspace.name
        )));
    }

    // 4. 各子项目 build（log_dir = workspace/logs/<dir>；分项目日志方便排查哪个构建失败）
    let enabled: Vec<_> = discovered
        .iter()
        .filter(|project| project.manifest.project.enabled)
        .collect();
    let mut built: Vec<BuiltProject> = Vec::with_capacity(enabled.len());
    for proj in enabled {
        let log_dir = ws.join("logs").join(&proj.dir);
        // path 安全校验 + 拼接（防 `../` 穿越 workspace）
        let proj_dir = crate::path_safety::ensure_within(&ws, &proj.dir).map_err(|_| {
            AppError::validation(format!(
                "project path escapes workspace: {} (=\"{}\")",
                proj.dir,
                proj.name()
            ))
        })?;
        if !proj_dir.is_dir() {
            return Err(AppError::resource(format!(
                "project dir not found: {} (path={})",
                proj.name(),
                proj.dir
            )));
        }
        let artifact = build_generic(
            build_manager,
            app_id,
            &proj.manifest.build.command,
            &proj_dir,
            &proj.manifest.build.artifact,
            &log_dir,
            timeout_secs,
        )
        .await?;
        built.push(BuiltProject {
            path: proj.dir.clone(),
            artifact,
        });
    }

    let release_id = uuid::Uuid::now_v7().simple().to_string();
    let pingap_version = required_release_metadata("RCODER_PINGAP_VERSION")?;
    let pingap_commit = required_release_metadata("RCODER_PINGAP_COMMIT")?;
    let runtime_image_digest = required_release_metadata("RCODER_RUNTIME_IMAGE_DIGEST")?;
    let lock = build_release_lock(
        &manifest,
        &discovered,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: &runtime_image_digest,
        },
    )
    .map_err(|e| AppError::business(e.to_string()))?;
    let file_name = format!("{WORKSPACE_PACKAGE_PREFIX}{release_id}.zip");
    let path = assemble_workspace_package(&ws, &built, &lock, &file_name).await?;
    let (sha256, size_bytes) = hash_file(&path).await?;
    Ok(WorkspaceBuildArtifact {
        release_id,
        path,
        file_name,
        sha256,
        size_bytes,
    })
}

fn required_release_metadata(name: &str) -> AppResult<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::system(format!(
                "required release metadata environment variable is missing: {name}"
            ))
        })
}

async fn hash_file(path: &std::path::Path) -> AppResult<(String, u64)> {
    use sha2::{Digest, Sha256};
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AppError::file(format!("read built workspace package: {e}")))?;
    let size = u64::try_from(bytes.len())
        .map_err(|e| AppError::system(format!("package size conversion: {e}")))?;
    Ok((hex::encode(Sha256::digest(&bytes)), size))
}

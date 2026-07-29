//! UserApp workspace 多项目打包：两级 manifest → 遍历子项目 build_generic → 组装整体包。
//!
//! - `app_id` = file-server `project_id`（复用 [`WorkspaceResolver::resolve_project`]，
//!   不新建 resolve_userapp）。workspace 根下有多个子项目（前端/后端/...）。
//! - file-server **只读** manifest 的 `[workspace]`+`[[projects]]`（驱动 build）和子项目的
//!   `[project]`+`[build]`；`[deploy]`（部署配置）由 Java `create_app` 读，file-server 不解析。
//! - 组装成一个整体包 `workspace-package.zip`：各子项目产物（加 `{path}/` 前缀）+ workspace 根
//!   `start.sh`/`scripts/` + pingap 反代配置 + `.service-ports`。详见设计文档 §5/§6.4。
//!
//! 子模块：
//! - [`manifest`]：两级 manifest 类型 + 解析
//! - [`assemble`]：整体包 zip 组装（raw copy 子产物 + 入口文件 + pingap 配置写入）
//! - [`pingap`]：pingap 反代配置（`pingap.toml`）+ `.service-ports` 生成（独立可扩展）

mod assemble;
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
use manifest::read_workspace_manifest;

/// 整体包产物文件名（放在 workspace 根，供 `GET /api/userapp/static` 下载）。
pub const WORKSPACE_PACKAGE_ZIP: &str = "workspace-package.zip";

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
/// 返回整体包绝对路径（`{workspace}/workspace-package.zip`）。`[deploy]` 不解析（Java 负责）。
pub async fn build_workspace_package(
    resolver: &dyn WorkspaceResolver,
    build_manager: &BuildManager,
    app_id: &str,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    timeout_secs: u64,
) -> AppResult<PathBuf> {
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

    // 2. workspace manifest（只读 [workspace].name，不再有 [[projects]]）
    let manifest = read_workspace_manifest(&ws).await?;

    // 3. 自动发现子项目（扫描含 project.manifest.toml 的一级子目录）
    let discovered = manifest::discover_projects(&ws).map_err(|e| {
        AppError::system(format!("discover projects in {}: {e}", ws.display()))
    })?;
    if discovered.is_empty() {
        return Err(AppError::business(format!(
            "no sub-projects found (no project.manifest.toml in any subdirectory of workspace=\"{}\")",
            manifest.workspace.name
        )));
    }

    // 4. 各子项目 build（log_dir = workspace/logs/<dir>；分项目日志方便排查哪个构建失败）
    let mut built: Vec<BuiltProject> = Vec::with_capacity(discovered.len());
    for proj in &discovered {
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
            &proj.manifest.build.cmd,
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

    // 4. 组装整体包
    assemble_workspace_package(&ws, &built).await
}

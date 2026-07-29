//! 读 workspace.manifest.toml + 各子项目 project.manifest.toml，组装待启动的服务清单。

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{ProjectManifest, ProjectRef, RunSection, WorkspaceManifest};

/// 子项目内部端口基（4000+i）。
pub const INTERNAL_PORT_BASE: u16 = 4000;

/// 一个待启动的子项目（含端口 + 启动命令 + 代理配置）。
pub struct ServiceSpec {
    pub project: ProjectRef,
    pub port: u16,
    pub run: RunSection,
}

/// 读 workspace.manifest.toml。
pub fn load_workspace(ws_root: &Path) -> Result<WorkspaceManifest> {
    let path = ws_root.join("workspace.manifest.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read workspace.manifest.toml: {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("parse workspace.manifest.toml: {}", path.display()))
}

/// 读单个子项目的 project.manifest.toml。
pub fn load_project(ws_root: &Path, project_path: &str) -> Result<ProjectManifest> {
    let path = ws_root.join(project_path).join("project.manifest.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read project.manifest.toml: {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("parse project.manifest.toml: {}", path.display()))
}

/// 从 workspace manifest + 各 project manifest 组装服务清单（按 [[projects]] 顺序，端口 4000+i）。
pub fn build_specs(ws_root: &Path, ws_manifest: &WorkspaceManifest) -> Result<Vec<ServiceSpec>> {
    let mut specs = Vec::with_capacity(ws_manifest.projects.len());
    for (i, p) in ws_manifest.projects.iter().enumerate() {
        let pm = load_project(ws_root, &p.path)?;
        specs.push(ServiceSpec {
            project: p.clone(),
            port: INTERNAL_PORT_BASE + i as u16,
            run: pm.run,
        });
    }
    Ok(specs)
}

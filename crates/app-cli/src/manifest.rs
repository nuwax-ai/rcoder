//! 自动发现子项目 + 组装服务清单。

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{discover_projects, DiscoveredProject, ProxySection, RunSection};

/// 子项目内部端口基（4000+i）。
pub const INTERNAL_PORT_BASE: u16 = 4000;

/// 一个待启动的子项目（含端口 + 启动命令 + 代理配置）。
pub struct ServiceSpec {
    /// 项目名（manifest [project].name 或目录名）。
    pub name: String,
    /// 目录名（workspace 相对路径，用于 cwd + 日志文件名）。
    pub dir: String,
    /// 容器内端口（4000+i，pingap 拨号到它）。
    pub port: u16,
    /// 启动命令。
    pub run: RunSection,
    /// 反代配置（None = 不经 pingap）。
    pub proxy: Option<ProxySection>,
}

/// 自动发现 workspace 下所有子项目 → 组装服务清单（按目录名字母序，端口 4000+i）。
pub fn build_specs(ws_root: &Path) -> Result<Vec<ServiceSpec>> {
    let discovered = discover_projects(ws_root)
        .with_context(|| format!("discover projects in {}", ws_root.display()))?;
    if discovered.is_empty() {
        anyhow::bail!("no sub-projects found (no project.manifest.toml in any subdirectory)");
    }
    Ok(discovered
        .into_iter()
        .enumerate()
        .map(|(i, p)| discovered_to_spec(p, INTERNAL_PORT_BASE + i as u16))
        .collect())
}

fn discovered_to_spec(d: DiscoveredProject, port: u16) -> ServiceSpec {
    ServiceSpec {
        name: d.name().to_string(),
        dir: d.dir,
        port,
        run: d.manifest.run,
        proxy: d.manifest.proxy,
    }
}

//! UserApp workspace 两级 manifest 类型。
//!
//! 极轻量独立 crate（仅 serde 依赖），供 `file-server`（build 侧）和 `app-cli`（runtime 侧）
//! 共享 manifest 类型，避免 app-cli（独立 workspace）拖入 file-server/shared_types 的重依赖。
//!
//! 消费方各自 `toml::from_str::<WorkspaceManifest>(...)` 解析。

use serde::Deserialize;

/// `workspace.manifest.toml`（workspace 根）。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceManifest {
    pub workspace: WorkspaceMeta,
    #[serde(default)]
    pub projects: Vec<ProjectRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceMeta {
    pub name: String,
}

/// workspace.manifest.toml 的 `[[projects]]` 条目。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectRef {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    pub proxy_strip_prefix: Option<bool>,
    #[serde(default)]
    pub proxy_cache: Option<bool>,
    #[serde(default)]
    pub proxy_rate_limit: Option<u32>,
}

/// `project.manifest.toml`（各子项目）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectManifest {
    pub project: ProjectMeta,
    pub build: BuildSection,
    #[serde(default)]
    pub run: RunSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    #[serde(default)]
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildSection {
    pub cmd: String,
    pub artifact: String,
}

/// project.manifest.toml 的 `[run]`（语言无关启动命令，app-cli 读它编排子项目）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunSection {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub migrate: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_workspace_manifest() {
        let toml_text = r#"
[workspace]
name = "ws"
[[projects]]
name = "frontend"
path = "fe"
proxy_path = "/"
[[projects]]
name = "backend"
path = "be"
proxy_path = "/api/"
proxy_strip_prefix = true
proxy_rate_limit = 600
"#;
        let m: WorkspaceManifest = toml::from_str(toml_text).expect("parse");
        assert_eq!(m.projects.len(), 2);
        assert_eq!(m.projects[0].proxy_path.as_deref(), Some("/"));
        assert_eq!(m.projects[1].proxy_rate_limit, Some(600));
    }

    #[test]
    fn parse_project_manifest_with_run() {
        let toml_text = r#"
[project]
name = "frontend"
type = "node"
[build]
cmd = "npm run build:standalone"
artifact = "fe.zip"
[run]
command = ["node", "server.js"]
migrate = ["node", "migrate.js"]
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse");
        assert_eq!(m.run.command, vec!["node".to_string(), "server.js".to_string()]);
    }
}

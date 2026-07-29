//! UserApp workspace 两级 manifest 类型 + 自动发现。
//!
//! 极轻量独立 crate（serde + toml），供 `file-server`（build 侧）和 `app-cli`（runtime 侧）共享。
//!
//! 设计：自动发现模式——workspace.manifest.toml 不再有 `[[projects]]` 列表，而是通过
//! [`discover_projects`] 扫描 workspace 根下含 `project.manifest.toml` 的一级子目录自动发现项目。
//! 每个项目自描述（[project]+[build]+[run]+[proxy]），增删项目 = 增删目录，无需改 workspace.manifest。

use std::path::Path;

use serde::Deserialize;

// ── workspace.manifest.toml（极简，无 [[projects]]）──────────────────────────────

/// `workspace.manifest.toml`（workspace 根）。只管 workspace 级元信息 + [deploy]。
/// 项目列表由 [`discover_projects`] 自动扫描得出。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceManifest {
    pub workspace: WorkspaceMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceMeta {
    pub name: String,
}

// ── project.manifest.toml（各子项目，完整自描述）─────────────────────────────────

/// `project.manifest.toml`（各子项目目录）。file-server 读 [build]；app-cli 读 [run]+[proxy]。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectManifest {
    pub project: ProjectMeta,
    pub build: BuildSection,
    #[serde(default)]
    pub run: RunSection,
    /// 反代配置（可选）。没有 [proxy] = 不经 pingap（仅内部服务）。
    #[serde(default)]
    pub proxy: Option<ProxySection>,
    /// 项目级环境变量（可选）。app-cli 启动子项目时注入（合并 workspace env，项目覆盖 workspace）。
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectMeta {
    /// 项目名（可选，缺省 = 目录名）。
    #[serde(default)]
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
    /// 健康检查路径（缺省 `/health`）。pingap upstream health_check 用它探活，不健康不路由。
    #[serde(default)]
    pub health: Option<String>,
    /// 就绪检查路径（可选，如 `/ready`）。区分「存活」(health) vs 「就绪」(ready)。
    #[serde(default)]
    pub ready: Option<String>,
}

/// project.manifest.toml 的 `[proxy]`（pingap 反代配置，从 workspace.manifest 下沉到此）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProxySection {
    /// pingap location 路径。`"/"` = 兜底 catch-all；`"/api/go/"` = 前缀匹配。
    pub path: String,
    /// 是否在转发前去前缀（rewrite `"^<path>(.*) /$1"`）。默认 false。
    #[serde(default)]
    pub strip_prefix: Option<bool>,
    /// 是否启用响应缓存。默认 false。
    #[serde(default)]
    pub cache: Option<bool>,
    /// IP 限流：每 60s 每 IP 最多 N 次。默认不限。
    #[serde(default)]
    pub rate_limit: Option<u32>,
}

// ── 自动发现 ─────────────────────────────────────────────────────────────────────

/// 自动发现的结果：一个子项目（目录名 + 完整 manifest）。
#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    /// 目录名（= workspace 相对路径，用于 build cwd + assemble 前缀 + 端口排序）。
    pub dir: String,
    /// 项目 manifest（[project]+[build]+[run]+[proxy]+[env]）。
    pub manifest: ProjectManifest,
}

impl DiscoveredProject {
    /// 项目名：manifest [project].name 非空则用，否则用目录名。
    pub fn name(&self) -> &str {
        let name = self.manifest.project.name.trim();
        if name.is_empty() {
            &self.dir
        } else {
            name
        }
    }
}

/// 扫描 `ws_root` 下所有含 `project.manifest.toml` 的一级子目录，按目录名字母序返回。
///
/// 约定：有 `project.manifest.toml` 的目录 = 项目；没有的（scripts/、logs/、.git/ 等）自动跳过。
/// 排序：目录名字母序，保证端口分配（4000+i）稳定可预测。
pub fn discover_projects(ws_root: &Path) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut projects = Vec::new();
    for entry in std::fs::read_dir(ws_root).map_err(|e| DiscoverError::ReadDir {
        path: ws_root.display().to_string(),
        source: e.to_string(),
    })? {
        let entry = entry.map_err(|e| DiscoverError::Io(e.to_string()))?;
        if !entry
            .file_type()
            .map_err(|e| DiscoverError::Io(e.to_string()))?
            .is_dir()
        {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        let manifest_path = entry.path().join("project.manifest.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
            DiscoverError::ReadManifest {
                path: manifest_path.display().to_string(),
                source: e.to_string(),
            }
        })?;
        let manifest: ProjectManifest = toml::from_str(&content).map_err(|e| {
            DiscoverError::ParseManifest {
                path: manifest_path.display().to_string(),
                source: e.to_string(),
            }
        })?;
        projects.push(DiscoveredProject {
            dir: dir_name,
            manifest,
        });
    }
    projects.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(projects)
}

/// 自动发现错误。
#[derive(Debug)]
pub enum DiscoverError {
    ReadDir { path: String, source: String },
    Io(String),
    ReadManifest { path: String, source: String },
    ParseManifest { path: String, source: String },
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverError::ReadDir { path, source } => {
                write!(f, "read dir {path}: {source}")
            }
            DiscoverError::Io(msg) => write!(f, "{msg}"),
            DiscoverError::ReadManifest { path, source } => {
                write!(f, "read {path}: {source}")
            }
            DiscoverError::ParseManifest { path, source } => {
                write!(f, "parse {path}: {source}")
            }
        }
    }
}

impl std::error::Error for DiscoverError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_workspace_manifest_no_projects() {
        let toml_text = r#"
[workspace]
name = "my-app"
"#;
        let m: WorkspaceManifest = toml::from_str(toml_text).expect("parse");
        assert_eq!(m.workspace.name, "my-app");
    }

    #[test]
    fn parse_project_manifest_with_proxy() {
        let toml_text = r#"
[project]
name = "backend-go"
type = "go"
[build]
cmd = "sh scripts/build-standalone.sh"
artifact = "artifact.zip"
[run]
command = ["./server"]
[proxy]
path = "/api/go/"
strip_prefix = true
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse");
        assert_eq!(m.project.name, "backend-go");
        assert_eq!(m.build.cmd, "sh scripts/build-standalone.sh");
        assert_eq!(m.run.command, vec!["./server".to_string()]);
        assert_eq!(
            m.proxy.as_ref().expect("proxy").path,
            "/api/go/"
        );
        assert_eq!(m.proxy.as_ref().unwrap().strip_prefix, Some(true));
    }

    #[test]
    fn parse_project_manifest_without_proxy() {
        let toml_text = r#"
[project]
name = "internal-worker"
[build]
cmd = "echo"
artifact = "x.zip"
[run]
command = ["./worker"]
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse");
        assert!(m.proxy.is_none());
    }

    #[test]
    fn parse_project_manifest_name_optional() {
        let toml_text = r#"
[project]
[build]
cmd = "echo"
artifact = "x.zip"
[run]
command = ["./app"]
"#;
        let m: ProjectManifest = toml::from_str(toml_text).expect("parse");
        assert!(m.project.name.is_empty());
    }

    #[test]
    fn discover_projects_sorts_by_dir_name() {
        let tmp = tempfile_dir();
        let root = tmp.path();

        // 故意逆序创建，验证排序
        create_project(root, "z-backend", "Go");
        create_project(root, "a-frontend", "Node");

        let projects = discover_projects(root).expect("discover");
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].dir, "a-frontend");
        assert_eq!(projects[1].dir, "z-backend");
    }

    #[test]
    fn discover_projects_skips_dirs_without_manifest() {
        let tmp = tempfile_dir();
        let root = tmp.path();

        create_project(root, "backend-go", "Go");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();

        let projects = discover_projects(root).expect("discover");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].dir, "backend-go");
    }

    #[test]
    fn discovered_project_name_fallbacks_to_dir() {
        let tmp = tempfile_dir();
        let root = tmp.path();

        // 有 name
        create_project(root, "dir-a", "Go");
        // 无 name
        fs::create_dir_all(root.join("dir-b")).unwrap();
        fs::write(
            root.join("dir-b/project.manifest.toml"),
            "[project]\n[build]\ncmd=\"echo\"\nartifact=\"x.zip\"\n[run]\ncommand=[\"./app\"]\n",
        )
        .unwrap();

        let projects = discover_projects(root).expect("discover");
        assert_eq!(projects[0].name(), "backend-go"); // from create_project helper
        assert_eq!(projects[1].name(), "dir-b"); // fallback to dir name
    }

    // ── helpers ──

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn create_project(root: &Path, dir: &str, project_type: &str) {
        let dir_path = root.join(dir);
        fs::create_dir_all(&dir_path).unwrap();
        let toml = format!(
            r#"[project]
name = "backend-{type}"
type = "{type}"
[build]
cmd = "echo"
artifact = "x.zip"
[run]
command = ["./server"]
[proxy]
path = "/api/{type}/"
"#,
            type = project_type.to_lowercase()
        );
        fs::write(dir_path.join("project.manifest.toml"), toml).unwrap();
    }
}

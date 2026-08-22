use std::path::Path;

use crate::{
    DiscoverError, DiscoveredProject, ProjectManifest, ValidationIssue, collect_topology_issues,
    manifest_file_of, parse_project, parse_project_toml, validate_project_at, validate_topology,
};

/// 扫描 workspace 根的一级子目录，发现并解析所有 `project.manifest.toml`。
///
/// 文件系统层：只负责"找目录 + 读文件 + 解析"；解析后的排序与拓扑校验交给
/// [`assemble_discovered`]——后者与文件系统无关，可被"重锁"（从 zip 内 manifest
/// 重建 release.lock.toml）等无文件系统场景复用。
pub fn discover_projects(ws_root: &Path) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<(String, ProjectManifest)> = Vec::new();
    for entry in std::fs::read_dir(ws_root).map_err(|error| DiscoverError::ReadDir {
        path: ws_root.display().to_string(),
        source: error.to_string(),
    })? {
        let entry = entry.map_err(|error| DiscoverError::Io(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| DiscoverError::Io(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let dir = entry.file_name().to_string_lossy().to_string();
        let path = entry.path().join("project.manifest.toml");
        if !path.is_file() {
            continue;
        }
        let content =
            std::fs::read_to_string(&path).map_err(|error| DiscoverError::ReadManifest {
                path: path.display().to_string(),
                source: error.to_string(),
            })?;
        let manifest = parse_project(&content).map_err(|error| DiscoverError::ParseManifest {
            path: path.display().to_string(),
            source: error.to_string(),
        })?;
        discovered.push((dir, manifest));
    }
    assemble_discovered(discovered)
}

/// 装配已解析的项目集合：按 `service_id` 排序 + 拓扑校验。
///
/// 与文件系统无关。既供 [`discover_projects`] 复用，也供未来 Stage 2 的
/// `relock_from_package`（从版本包 zip 内的 manifest 重锁，无文件系统访问）复用。
pub fn assemble_discovered(
    projects: Vec<(String, ProjectManifest)>,
) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<DiscoveredProject> = projects
        .into_iter()
        .map(|(dir, manifest)| DiscoveredProject { dir, manifest })
        .collect();
    discovered.sort_by(|a, b| a.service_id().cmp(b.service_id()));
    validate_topology(&discovered).map_err(|error| DiscoverError::Validation(error.to_string()))?;
    Ok(discovered)
}

/// 宽松发现：扫描全部模块，**解析/校验失败不中断**，全部收集为 issue 返回。
///
/// 供 app-cli devtool / 诊断入口呈现"一次看全"的错误清单（fast-fail 版
/// [`discover_projects`] 只报第一个错，适合构建链快速失败）。
/// TOML 解析错误自带行列号（`toml` crate），以 `<dir>/project.manifest.toml` 定位。
pub fn discover_projects_lenient(
    ws_root: &Path,
) -> Result<(Vec<DiscoveredProject>, Vec<ValidationIssue>), DiscoverError> {
    let mut discovered: Vec<(String, ProjectManifest)> = Vec::new();
    let mut issues = Vec::new();
    let dir_entries = std::fs::read_dir(ws_root)
        .map_err(|error| DiscoverError::ReadDir {
            path: ws_root.display().to_string(),
            source: error.to_string(),
        })?
        .collect::<Vec<_>>();
    // 枚举 IO 错误收集为 issue（不静默吞掉——模块缺失会引发 depends_on 连锁误报）。
    let mut entries: Vec<_> = dir_entries
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                issues.push(
                    ValidationIssue::new(format!("directory entry read failed: {error}"))
                        .at_file(ws_root.display().to_string()),
                );
                None
            }
        })
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| {
            let dir = entry.file_name().to_string_lossy().to_string();
            (dir, entry.path().join("project.manifest.toml"))
        })
        .filter(|(_, path)| path.is_file())
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (dir, path) in entries {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                issues.push(
                    ValidationIssue::new(format!("cannot read manifest: {error}"))
                        .at_file(manifest_file_of(&dir)),
                );
                continue;
            }
        };
        // 仅反序列化：语法错误与语义校验分开呈现（语义问题由收集式校验统一
        // 四段式渲染，避免对已含 field/fix 的文本再包一层通用提示）。
        match parse_project_toml(&content) {
            Ok(manifest) => {
                let file_issues = validate_project_at(&manifest, &dir);
                if file_issues.is_empty() {
                    discovered.push((dir, manifest));
                } else {
                    issues.extend(file_issues);
                }
            }
            Err(error) => issues.push(
                ValidationIssue::new(error.to_string())
                    .at_file(manifest_file_of(&dir))
                    .with_hint("fix the TOML syntax/type error at the reported line, then re-run"),
            ),
        }
    }
    // 单文件全过的模块间拓扑校验（service_id/proxy 路由/依赖）。
    let mut all: Vec<DiscoveredProject> = discovered
        .into_iter()
        .map(|(dir, manifest)| DiscoveredProject { dir, manifest })
        .collect();
    all.sort_by(|a, b| a.service_id().cmp(b.service_id()));
    issues.extend(collect_topology_issues(&all));
    Ok((all, issues))
}

//! 构建产物包搜索与解析 (对齐 nuwax `findPackageScript` / `extractPlatformFromFileName`)。
//!
//! 从 handlers 层下沉: 纯 FS 搜索 + stdout/文件名解析, 无 axum/HTTP 依赖。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// 递归找含 `manifest` 的最近目录, 返回该目录 (BFS)。
pub async fn find_first(root: &Path, manifest: &str) -> Option<PathBuf> {
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
pub fn package_search_skip_dirs(zip_workspace_exclude: &[String]) -> Vec<String> {
    let mut v = zip_workspace_exclude.to_vec();
    if !v.iter().any(|d| d == "dist-packages") {
        v.push("dist-packages".to_string());
    }
    v
}

/// 递归查找含 `scripts/package-platforms.mjs` 的目录 (对齐 nuwax findPackageScript)。
/// 深度优先, 跳过 skip_dirs 命中的目录名。
pub async fn find_package_script(root: &Path, skip_dirs: &[String]) -> Option<PathBuf> {
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
pub fn parse_artifacts(stdout: &str, workspace: &Path) -> Vec<Value> {
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
pub(super) fn extract_platform_from_filename(file_name: &str) -> Option<String> {
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
    use super::extract_platform_from_filename;

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

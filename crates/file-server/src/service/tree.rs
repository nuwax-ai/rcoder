//! 文件树遍历 (对齐 nuwax `getContentUtils.traverseDirectory` + `getProjectContent`)。
//!
//! 返回扁平数组 (非嵌套树): 非空目录的子文件直接展开, 仅空目录产生 `{isDir:true}` 节点。

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use tokio::fs;

use crate::config::Config;
use crate::error::AppResult;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_exceeded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_link: Option<bool>,
}

#[derive(Serialize)]
pub struct ProjectContent {
    pub files: Vec<FileEntry>,
    pub frontend_framework: String,
    pub dev_framework: String,
}

pub async fn get_project_content(
    project_path: &Path,
    config: &Config,
    command: Option<&str>,
    proxy_path: Option<&str>,
) -> AppResult<ProjectContent> {
    let mut files = Vec::new();
    traverse(project_path, project_path, config, proxy_path, &mut files).await?;
    // 非 cpage_config 命令时过滤掉 cpage_config.json
    if command != Some("cpage_config") {
        files.retain(|f| f.name != "cpage_config.json");
    }
    let (frontend_framework, dev_framework) = detect_framework(project_path).await;
    Ok(ProjectContent {
        files,
        frontend_framework,
        dev_framework,
    })
}

/// 纯遍历 (不 filter cpage_config / 不 detect framework), 供 get-by-version 复用。
pub async fn list_files(
    root: &Path,
    config: &Config,
    proxy_path: Option<&str>,
) -> AppResult<Vec<FileEntry>> {
    let mut files = Vec::new();
    traverse(root, root, config, proxy_path, &mut files).await?;
    Ok(files)
}

/// 轻量元信息遍历 (对齐 nuwax computer `traverseDirectory`): **不读文件内容**,
/// 仅返回 `{name, isDir, fileProxyUrl, isLink}` (binary/sizeExceeded/contents 均省略)。
/// 供 computer get-file-list 使用 (避免为列目录读取全部文件内容)。
pub async fn list_files_meta(
    root: &Path,
    config: &Config,
    proxy_path: Option<&str>,
) -> AppResult<Vec<FileEntry>> {
    let mut files = Vec::new();
    traverse_meta(root, root, config, proxy_path, &mut files).await?;
    Ok(files)
}

async fn traverse_meta(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let mut entries = fs::read_dir(dir).await?;
    let mut items: Vec<(String, PathBuf, bool, bool)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 隐藏文件 (除 .gitignore) 跳过 (对齐 nuwax computer traverseDirectory)
        if name.starts_with('.') && name != ".gitignore" {
            continue;
        }
        let ft = entry.file_type().await?;
        let path = entry.path();
        let is_link = ft.is_symlink();
        if ft.is_dir() {
            if config.traverse_exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            items.push((name, path, true, is_link));
        } else if ft.is_file() {
            if config
                .content_traverse_exclude_files
                .iter()
                .any(|f| f == &name)
            {
                continue;
            }
            items.push((name, path, false, is_link));
        }
    }
    // 排序: 目录在前, 名字大小写不敏感 (对齐 nuwax localeCompare)
    items.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    for (_name, path, is_dir, is_link) in items {
        let relative = relative_path(root, &path);
        if is_dir {
            let mut sub = Vec::new();
            Box::pin(traverse_meta(root, &path, config, proxy_path, &mut sub)).await?;
            if sub.is_empty() {
                out.push(FileEntry {
                    name: relative,
                    is_dir: true,
                    binary: None,
                    size_exceeded: None,
                    contents: None,
                    file_proxy_url: None,
                    is_link: Some(is_link),
                });
            } else {
                out.extend(sub);
            }
        } else {
            out.push(FileEntry {
                name: relative.to_string(),
                is_dir: false,
                binary: None,
                size_exceeded: None,
                contents: None,
                file_proxy_url: proxy_path
                    .map(|p| format!("{p}/{}", encode_path_segments(&relative))),
                is_link: Some(is_link),
            });
        }
    }
    Ok(())
}

async fn traverse(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let mut entries = fs::read_dir(dir).await?;
    let mut items: Vec<(String, PathBuf, bool)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 隐藏文件 (除 .gitignore) 跳过
        if name.starts_with('.') && name != ".gitignore" {
            continue;
        }
        let ft = entry.file_type().await?;
        let path = entry.path();
        if ft.is_dir() {
            if config.traverse_exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            items.push((name, path, true));
        } else if ft.is_file() {
            if config
                .content_traverse_exclude_files
                .iter()
                .any(|f| f == &name)
            {
                continue;
            }
            items.push((name, path, false));
        }
    }
    // 排序: 目录在前, 名字大小写不敏感比较 (对齐 nuwax localeCompare)
    items.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    for (_name, path, is_dir) in items {
        let relative = relative_path(root, &path);
        if is_dir {
            let mut sub = Vec::new();
            Box::pin(traverse(root, &path, config, proxy_path, &mut sub)).await?;
            if sub.is_empty() {
                // 空目录才产生节点 (非空目录的子文件已展开)
                out.push(FileEntry {
                    name: relative,
                    is_dir: true,
                    binary: None,
                    size_exceeded: None,
                    contents: None,
                    file_proxy_url: None,
                    is_link: None,
                });
            } else {
                out.extend(sub);
            }
        } else {
            out.push(build_file_entry(&path, &relative, config, proxy_path).await?);
        }
    }
    Ok(())
}

async fn build_file_entry(
    path: &Path,
    relative: &str,
    config: &Config,
    proxy_path: Option<&str>,
) -> AppResult<FileEntry> {
    let metadata = fs::metadata(path).await?;
    let size = metadata.len();
    let size_exceeded = size > config.max_inline_file_size_bytes;

    let mut binary = None;
    let mut contents = None;

    if !size_exceeded && size > 0 {
        let bytes = fs::read(path).await?;
        let is_bin = is_binary(&bytes);
        binary = Some(is_bin);
        if !is_bin {
            contents = Some(String::from_utf8_lossy(&bytes).into_owned());
        } else if is_image(path, &config.inline_image_extensions) {
            // 图片二进制 → base64
            contents = Some(base64::engine::general_purpose::STANDARD.encode(&bytes));
        }
    }

    Ok(FileEntry {
        name: relative.to_string(),
        is_dir: false,
        binary,
        size_exceeded: Some(size_exceeded),
        contents,
        file_proxy_url: proxy_path.map(|p| format!("{p}/{relative}")),
        is_link: None,
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

/// 相对路径逐段 encodeURIComponent (对齐 nuwax traverseDirectory 的 fileProxyUrl 构造:
/// `relativePath.split("/").map(encodeURIComponent).join("/")`)。
fn encode_path_segments(rel: &str) -> String {
    rel.split('/')
        .map(crate::service::code::encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// 二进制检测: 含 NUL 或控制字符 (除 \t \n \r) → 二进制 (对齐 nuwax isBinaryFile)。
fn is_binary(bytes: &[u8]) -> bool {
    for &b in bytes {
        if b == 0 {
            return true;
        }
        if b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r' {
            return true;
        }
    }
    false
}

fn is_image(path: &Path, exts: &[String]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let dot = format!(".{ext}");
            exts.iter().any(|e| e.eq_ignore_ascii_case(&dot))
        }
        None => false,
    }
}

/// 框架检测 (对齐 nuwax frameworkDetectorUtils):
/// - frontend: react 优先; vue/vue-router/@vue/cli-service → 按版本号 vue2/vue3/vue
/// - devFramework: 只看配置文件, nextjs 优先于 vite
async fn detect_framework(project_path: &Path) -> (String, String) {
    let frontend = detect_frontend_framework(project_path).await;
    let dev = detect_dev_framework(project_path).await;
    (frontend, dev)
}

/// frontend 检测 (对齐 nuwax detectFrontendFramework):
/// react/react-dom → "react"; vue 系 → parse_vue_major_version 取首个可解析版本 → "vue2"/"vue3"/"vue"; 否则 "other"。
async fn detect_frontend_framework(project_path: &Path) -> String {
    let pkg_path = project_path.join("package.json");
    let Ok(text) = fs::read_to_string(&pkg_path).await else {
        return "other".to_string();
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    // 合并 dependencies + devDependencies (对齐 nuwax)
    let merged = merge_dep_map(&v);
    // react 优先
    if merged.contains_key("react") || merged.contains_key("react-dom") {
        return "react".to_string();
    }
    // vue 系: 依次对 vue / vue-router / @vue/cli-service 取首个可解析主版本
    if merged.contains_key("vue")
        || merged.contains_key("vue-router")
        || merged.contains_key("@vue/cli-service")
    {
        for key in ["vue", "vue-router", "@vue/cli-service"] {
            if let Some(ver) = merged.get(key)
                && let Some(major) = parse_vue_major_version(ver)
            {
                return format!("vue{major}");
            }
        }
        return "vue".to_string();
    }
    "other".to_string()
}

/// devFramework 检测 (对齐 nuwax detectDevFramework): 只看配置文件, nextjs 优先。
async fn detect_dev_framework(project_path: &Path) -> String {
    for f in [
        "next.config.js",
        "next.config.ts",
        "next.config.mjs",
        "next.config.cjs",
    ] {
        if fs::try_exists(project_path.join(f)).await.unwrap_or(false) {
            return "nextjs".to_string();
        }
    }
    for f in [
        "vite.config.js",
        "vite.config.ts",
        "vite.config.mjs",
        "vite.config.cjs",
    ] {
        if fs::try_exists(project_path.join(f)).await.unwrap_or(false) {
            return "vite".to_string();
        }
    }
    "other".to_string()
}

/// 合并 dependencies + devDependencies 为 {name: version} 映射 (对齐 nuwax 合并范围)。
fn merge_dep_map(pkg: &serde_json::Value) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(obj) = pkg.get(key).and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    map.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    map
}

/// 解析 vue 依赖主版本 (对齐 nuwax parseVueMajorVersion):
/// - 处理 npm alias: `npm:vue@^3.4.0` → `^3.4.0`
/// - 正则 `(?:^|[^\d])v?(\d+)(?:\.|x|\b)` 取首个独立数字段
/// - 非标准 (workspace/file/git/url) 返回 None
fn parse_vue_major_version(raw: &str) -> Option<u32> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    // npm alias: npm:<name>@<version>
    let s = if let Some(rest) = regex_alias_version(&s) {
        rest.to_string()
    } else {
        s
    };
    // 排除非标准来源
    if s.starts_with("workspace:")
        || s.starts_with("file:")
        || s.starts_with("git:")
        || s.starts_with("git+")
        || s.starts_with("http:")
        || s.starts_with("https:")
    {
        return None;
    }
    let re = regex::Regex::new(r"(?:^|[^\d])v?(\d+)(?:[\.x]|\b)").ok()?;
    re.captures(&s)
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
}

/// 提取 `npm:pkg@<version>` 中的 `<version>` 部分。
fn regex_alias_version(s: &str) -> Option<&str> {
    let re = regex::Regex::new(r"^npm:[^@]+@(.+)$").ok()?;
    re.captures(s).and_then(|c| c.get(1).map(|m| m.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vue_major_version_handles_caret_tilde() {
        assert_eq!(parse_vue_major_version("^3.4.0"), Some(3));
        assert_eq!(parse_vue_major_version("~2.7.16"), Some(2));
        assert_eq!(parse_vue_major_version("3.0.0"), Some(3));
        assert_eq!(parse_vue_major_version("v3.2.0"), Some(3));
    }

    #[test]
    fn parse_vue_major_version_handles_x_and_plain() {
        assert_eq!(parse_vue_major_version("2.x"), Some(2));
        assert_eq!(parse_vue_major_version("3"), Some(3));
    }

    #[test]
    fn parse_vue_major_version_handles_npm_alias() {
        assert_eq!(parse_vue_major_version("npm:vue@^3.4.0"), Some(3));
        assert_eq!(parse_vue_major_version("npm:vue@~2.7.0"), Some(2));
    }

    #[test]
    fn parse_vue_major_version_rejects_nonstandard() {
        assert_eq!(parse_vue_major_version("workspace:*"), None);
        assert_eq!(parse_vue_major_version("file:../vue"), None);
        assert_eq!(parse_vue_major_version("git+https://x"), None);
        assert_eq!(parse_vue_major_version(""), None);
    }
}

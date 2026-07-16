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
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
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

/// 框架检测 (对齐 nuwax frameworkDetectorUtils): 读 package.json dependencies。
async fn detect_framework(project_path: &Path) -> (String, String) {
    let pkg_path = project_path.join("package.json");
    let (frontend, dev) = match fs::read_to_string(&pkg_path).await {
        Ok(text) => {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let deps = collect_dep_names(&v);
            let frontend = if deps.iter().any(|d| d == "react" || d == "react-dom") {
                "react"
            } else if deps.iter().any(|d| d == "vue") {
                "vue3"
            } else {
                "other"
            };
            let dev = if deps.iter().any(|d| d == "vite") {
                "vite"
            } else if deps.iter().any(|d| d == "next") {
                "nextjs"
            } else {
                "other"
            };
            (frontend.to_string(), dev.to_string())
        }
        Err(_) => ("other".to_string(), "other".to_string()),
    };
    (frontend, dev)
}

fn collect_dep_names(pkg: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = pkg.get(key).and_then(|v| v.as_object()) {
            for k in obj.keys() {
                names.push(k.clone());
            }
        }
    }
    names
}

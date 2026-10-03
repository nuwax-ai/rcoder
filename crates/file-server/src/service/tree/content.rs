//! project 内容读取与版本文件列表，保持既有过滤和内联内容语义。

use std::path::Path;
use std::time::Instant;

use base64::Engine;
use tokio::fs;

use crate::config::Config;
use crate::error::AppResult;

use super::entries::{DirectoryExcludePolicy, read_filtered_entries};
use super::{FileEntry, ProjectContent, framework};

pub async fn get_project_content(
    project_path: &Path,
    config: &Config,
    command: Option<&str>,
    proxy_path: Option<&str>,
) -> AppResult<ProjectContent> {
    let start = Instant::now();
    let mut files = Vec::new();
    traverse(project_path, project_path, config, proxy_path, &mut files).await?;
    // 非 cpage_config 命令时过滤掉 cpage_config.json
    if command != Some("cpage_config") {
        files.retain(|f| f.name != "cpage_config.json");
    }
    let (frontend_framework, dev_framework) = framework::detect_framework(project_path).await?;
    tracing::info!(
        op = "get_project_content",
        elapsed_ms = start.elapsed().as_millis(),
        file_count = files.len(),
        "project content traversal completed"
    );
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
    let start = Instant::now();
    let mut files = Vec::new();
    traverse(root, root, config, proxy_path, &mut files).await?;
    tracing::info!(
        op = "list_files",
        elapsed_ms = start.elapsed().as_millis(),
        file_count = files.len(),
        "file traversal completed"
    );
    Ok(files)
}

/// 计算相对 `root` 的 POSIX 风格路径。
fn make_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

async fn traverse(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    // 复用 read_filtered_entries (过滤 + 排序)，但不跟随 symlink 读取项目文件内容。
    let items = read_filtered_entries(dir, config, DirectoryExcludePolicy::Exact).await?;

    for (_name, path, is_dir, is_link) in items {
        if is_link {
            continue;
        }
        let relative = make_relative_path(root, &path);
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

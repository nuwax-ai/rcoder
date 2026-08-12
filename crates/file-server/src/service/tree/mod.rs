//! 文件树遍历 (对齐 nuwax `getContentUtils.traverseDirectory` + `getProjectContent`)。
//!
//! 返回扁平数组 (非嵌套树): 非空目录的子文件直接展开, 仅空目录产生 `{isDir:true}` 节点。
//!
//! 模块拆分:
//! - 本 mod.rs: 共享类型 (`FileEntry`/`ProjectContent`) + 遍历函数 + 共享工具
//! - `framework`: 前端/构建框架检测
//! - `resolve`: resolve-file 路径解析
//! - `search`: 无索引有界实时搜索

mod framework;
pub mod resolve;
pub mod search;

pub use resolve::{FileResolveResult, resolve_existing_file};
pub use search::{SearchParams, SearchResult, search_files};

use std::path::{Component, Path, PathBuf};

use base64::Engine;
use path_clean::PathClean;
use serde::Serialize;
use tokio::fs;

use crate::config::Config;

/// 遍历时保留的唯一隐藏文件 (其余 `.` 开头的文件跳过)。
const KEEP_HIDDEN_FILE: &str = ".gitignore";
use crate::error::{AppError, AppResult};
use crate::path_safety;

#[derive(Serialize, Debug)]
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
    let (frontend_framework, dev_framework) = framework::detect_framework(project_path).await?;
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
///
/// - `relative_path`: 相对 `root` 的子目录 (可多级), `None`/空 → 列 `root` 本身 (对齐 TS `relativePath`)。
/// - `recursive`: `true` (默认, 原全量递归) / `false` 仅当前一层 (对齐 TS `listDirectoryLevel`)。
///
/// 路径越界 (含绝对路径注入 / `..` 穿越) 返回 `Err` (对齐 TS `resolvePathWithinWorkspace` 抛 ValidationError)。
pub async fn list_files_meta(
    root: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    relative_path: Option<&str>,
    recursive: bool,
) -> AppResult<Vec<FileEntry>> {
    let list_dir = resolve_subdir(root, relative_path)?;
    if !crate::service::fs_util::path_exists(&list_dir).await? {
        // 目录不存在 → 空数组 (handler 层也会先判存在, 这里是防御)
        return Ok(Vec::new());
    }
    let meta = fs::metadata(&list_dir).await?;
    if !meta.is_dir() {
        return Err(AppError::validation("relativePath must be a directory"));
    }
    let mut files = Vec::new();
    if recursive {
        traverse_meta(root, &list_dir, config, proxy_path, &mut files).await?;
    } else {
        list_directory_level(root, &list_dir, config, proxy_path, &mut files).await?;
    }
    Ok(files)
}

// ── 共享工具 (供子模块 resolve/search 复用) ────────────────────────────────────

/// 解析 `relative_path` 到 `root` 内的子目录绝对路径 (对齐 TS `resolvePathWithinWorkspace`)。
///
/// 用 [`path_clean::PathClean`] 标准化路径 (等价 TS `path.normalize`), 消除 `.`/`..`:
/// - `None` / `""` / `"."` / `"/"` → `root` 本身;
/// - 前导 `/` 剥离 → 兼容 `"/sub"` 这类写法 (对齐 TS `replace(/^[\/\\]+/,"")`);
/// - 标准化后仍含 `..` (即 `..` 未被抵消, 越出根) → `Err`;
/// - `ensure_within_path` (clean + starts_with) 做最终兜底, 双重保险。
///
/// 最终经 [`path_safety::ensure_within_path`] (clean + starts_with) 兜底, 双重保险。
pub(super) fn resolve_subdir(root: &Path, relative_path: Option<&str>) -> AppResult<PathBuf> {
    let Some(rel) = relative_path.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(root.clean());
    };

    // 剥前导斜杠 (对齐 TS replace(/^[\/\\]+/,"")), 兼容 "/sub" 写法。
    let stripped = rel.trim_start_matches(['/', '\\']);
    if stripped.is_empty() {
        return Ok(root.clean());
    }

    // 标准化: 消除 . 和能抵消的 .. (如 "a/../b" → "b")。未抵消的 .. 会保留。
    let normalized = Path::new(stripped).clean();

    // 标准化后仍含 .. → 越界 (如 "../x" 不会被抵消)。用 components 检测最可靠。
    if normalized
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(AppError::validation(
            "relativePath is not safe, cannot exceed target directory",
        ));
    }

    // normalized 是相对路径 (已剥前导斜杠 + 无 ..), join 不会替换 base;
    // ensure_within_path 做最终 starts_with 兜底, 双重保险。
    path_safety::ensure_within_path(&root.clean(), normalized)
}

/// 读取目录条目并按 nuwax 规则过滤 + 排序 (隐藏文件除 .gitignore / traverse_exclude_dirs /
/// content_traverse_exclude_files; 目录在前 + 名字大小写不敏感)。供递归/单层遍历复用。
/// 跳过既非目录也非文件的条目 (如符号链接断链), 对齐 TS `isDirectory()/isFile()` 行为。
async fn read_filtered_entries(
    dir: &Path,
    config: &Config,
) -> AppResult<Vec<(String, PathBuf, bool, bool)>> {
    let mut entries = fs::read_dir(dir).await?;
    let mut items: Vec<(String, PathBuf, bool, bool)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 隐藏文件 (除 .gitignore) 跳过 (对齐 nuwax computer traverseDirectory)
        if name.starts_with('.') && name != KEEP_HIDDEN_FILE {
            continue;
        }
        let ft = entry.file_type().await?;
        let path = entry.path();
        let is_link = ft.is_symlink();
        if ft.is_dir() && !config.traverse_exclude_dirs.iter().any(|d| d == &name) {
            items.push((name, path, true, is_link));
        } else if ft.is_file()
            && !config
                .content_traverse_exclude_files
                .iter()
                .any(|f| f == &name)
        {
            items.push((name, path, false, is_link));
        }
    }
    // 排序: 目录在前, 名字大小写不敏感 (对齐 nuwax localeCompare)
    items.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    Ok(items)
}

/// 计算相对 `root` 的 POSIX 风格路径。
fn make_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

/// 构造 fileProxyUrl 的 path 段 (对齐 TS `buildFileProxyUrl` 的 path 部分):
/// `${proxyPath}/${逐段 enc}`。customTargetDir 后缀由 handler 统一追加。
/// `proxy_path`/`relative` 为空 → `None`。
pub(super) fn build_file_proxy_url(proxy_path: Option<&str>, relative: &str) -> Option<String> {
    let p = proxy_path?;
    if relative.is_empty() {
        return None;
    }
    Some(format!("{p}/{}", encode_path_segments(relative)))
}

// ── 内部遍历实现 ────────────────────────────────────────────────────────────────

/// 单层遍历 (对齐 nuwax computer `listDirectoryLevel`): 仅列出 `dir` 下一层条目, 不递归。
/// 空目录 (无任何可见条目) 不会产生节点 (TS listDirectoryLevel 同样不返回空目录自身)。
async fn list_directory_level(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let items = read_filtered_entries(dir, config).await?;
    for (_name, path, is_dir, is_link) in items {
        let relative = make_relative_path(root, &path);
        if is_dir {
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
            out.push(FileEntry {
                name: relative.to_string(),
                is_dir: false,
                binary: None,
                size_exceeded: None,
                contents: None,
                file_proxy_url: build_file_proxy_url(proxy_path, &relative),
                is_link: Some(is_link),
            });
        }
    }
    Ok(())
}

async fn traverse_meta(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let items = read_filtered_entries(dir, config).await?;
    for (_name, path, is_dir, is_link) in items {
        let relative = make_relative_path(root, &path);
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
                file_proxy_url: build_file_proxy_url(proxy_path, &relative),
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
    // 复用 read_filtered_entries (过滤 + 排序), 丢弃 is_link (traverse 不需要)。
    let items = read_filtered_entries(dir, config).await?;

    for (_name, path, is_dir, _is_link) in items {
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

// ── 测试 ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_test_config() -> Config {
        Config::default()
    }

    /// 构造测试目录结构 (供 list_files_meta / resolve / search 测试共享):
    /// ```text
    /// root/
    ///   a.txt
    ///   b.md
    ///   sub/
    ///     c.txt
    ///     d.log
    ///     nested/
    ///       e.txt
    /// ```
    async fn make_test_tree(root: &Path) {
        fs::create_dir_all(root.join("sub").join("nested"))
            .await
            .unwrap();
        fs::write(root.join("a.txt"), "a").await.unwrap();
        fs::write(root.join("b.md"), "b").await.unwrap();
        fs::write(root.join("sub").join("c.txt"), "c")
            .await
            .unwrap();
        fs::write(root.join("sub").join("d.log"), "d")
            .await
            .unwrap();
        fs::write(root.join("sub").join("nested").join("e.txt"), "e")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_files_meta_recursive_flattens_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        let files = list_files_meta(tmp.path(), &cfg, None, None, true)
            .await
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        // 递归: 所有文件扁平展开
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.md"));
        assert!(names.contains(&"sub/c.txt"));
        assert!(names.contains(&"sub/d.log"));
        assert!(names.contains(&"sub/nested/e.txt"));
    }

    #[tokio::test]
    async fn list_files_meta_single_level_lists_only_immediate_children() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        let files = list_files_meta(tmp.path(), &cfg, None, None, false)
            .await
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        // 单层: 仅根目录直接子项
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.md"));
        assert!(names.contains(&"sub")); // 子目录作为节点
        // 不应包含孙子层
        assert!(!names.contains(&"sub/c.txt"));
        assert!(!names.contains(&"sub/nested/e.txt"));
    }

    #[tokio::test]
    async fn list_files_meta_with_relative_path_lists_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // relative_path="sub" + recursive=false → 仅 sub 一层
        let files = list_files_meta(tmp.path(), &cfg, None, Some("sub"), false)
            .await
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"sub/c.txt"));
        assert!(names.contains(&"sub/d.log"));
        assert!(names.contains(&"sub/nested"));
        // 不含根层文件
        assert!(!names.contains(&"a.txt"));
    }

    #[tokio::test]
    async fn list_files_meta_relative_path_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        let err = list_files_meta(tmp.path(), &cfg, None, Some("../escape"), true)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(..)));
    }

    #[tokio::test]
    async fn list_files_meta_relative_path_not_directory_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // a.txt 是文件, 不是目录
        let err = list_files_meta(tmp.path(), &cfg, None, Some("a.txt"), true)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(..)));
    }

    #[tokio::test]
    async fn list_files_meta_proxy_url_encoded() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a b.txt"), "x").await.unwrap();
        let cfg = default_test_config();
        let files = list_files_meta(tmp.path(), &cfg, Some("/proxy"), None, false)
            .await
            .unwrap();
        let entry = files.iter().find(|f| f.name == "a b.txt").unwrap();
        // 空格应被 encode → %20
        assert_eq!(entry.file_proxy_url.as_deref(), Some("/proxy/a%20b.txt"));
    }

    // ── resolve_subdir 单元测试 (components 语义, 不依赖文件系统) ──────────────

    #[test]
    fn resolve_subdir_none_and_empty_return_root() {
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, None).unwrap(),
            PathBuf::from("/app/ws")
        );
        assert_eq!(
            resolve_subdir(root, Some("")).unwrap(),
            PathBuf::from("/app/ws")
        );
        assert_eq!(
            resolve_subdir(root, Some("   ")).unwrap(),
            PathBuf::from("/app/ws")
        );
    }

    #[test]
    fn resolve_subdir_dot_and_slash_return_root() {
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, Some(".")).unwrap(),
            PathBuf::from("/app/ws")
        );
        assert_eq!(
            resolve_subdir(root, Some("/")).unwrap(),
            PathBuf::from("/app/ws")
        );
    }

    #[test]
    fn resolve_subdir_normal_path_resolved_under_root() {
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, Some("sub")).unwrap(),
            PathBuf::from("/app/ws/sub")
        );
        assert_eq!(
            resolve_subdir(root, Some("a/b/c")).unwrap(),
            PathBuf::from("/app/ws/a/b/c")
        );
    }

    #[test]
    fn resolve_subdir_strips_leading_slash() {
        // 对齐 TS: "/sub" 应兼容为 root/sub (剥前导斜杠), 而非被当绝对路径拒绝
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, Some("/sub")).unwrap(),
            PathBuf::from("/app/ws/sub")
        );
        assert_eq!(
            resolve_subdir(root, Some("/a/b")).unwrap(),
            PathBuf::from("/app/ws/a/b")
        );
    }

    #[test]
    fn resolve_subdir_cur_dir_skipped() {
        // "./sub" → CurDir 跳过, 只留 Normal("sub")
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, Some("./sub")).unwrap(),
            PathBuf::from("/app/ws/sub")
        );
        // "a/./b" → CurDir 跳过
        assert_eq!(
            resolve_subdir(root, Some("a/./b")).unwrap(),
            PathBuf::from("/app/ws/a/b")
        );
    }

    #[test]
    fn resolve_subdir_parent_dir_cancels_normal() {
        // a/../b → .. 抵消 a, 归约为 b (合法, 对齐 TS path.normalize)
        let root = Path::new("/app/ws");
        assert_eq!(
            resolve_subdir(root, Some("a/../b")).unwrap(),
            PathBuf::from("/app/ws/b")
        );
        // a/./../b → . 跳过, .. 抵消 a → b
        assert_eq!(
            resolve_subdir(root, Some("a/./../b")).unwrap(),
            PathBuf::from("/app/ws/b")
        );
        // ./.. → CurDir 跳过, ParentDir 栈空 → 越界拒绝
        assert!(resolve_subdir(root, Some("./..")).is_err());
    }

    #[test]
    fn resolve_subdir_parent_dir_overflow_rejected() {
        // .. 超过前面的 Normal 段数 = 越界 (栈空仍要 pop)
        let root = Path::new("/app/ws");
        assert!(resolve_subdir(root, Some("../escape")).is_err());
        assert!(resolve_subdir(root, Some("a/../../escape")).is_err());
        // a/../b/../../x: 抵消后 b 还剩, 再 .. 栈空 → 越界
        assert!(resolve_subdir(root, Some("a/../b/../../x")).is_err());
    }
}

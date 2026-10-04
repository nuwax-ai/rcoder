//! computer 轻量文件列表：目录条目、类型/深度/数量筛选及链接安全过滤。

use std::path::Path;
use std::time::Instant;

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};

use super::entries::{DirectoryExcludePolicy, read_filtered_entries};
use super::{
    FileEntry, MetaListOptions, MetaListType, build_file_proxy_url, ensure_resolved_list_dir,
    resolve_subdir,
};

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
    list_files_meta_filtered(
        root,
        config,
        proxy_path,
        relative_path,
        MetaListOptions {
            recursive,
            levels_left: 1,
            file_type: MetaListType::All,
            limit: None,
        },
    )
    .await
}

/// `get-file-list` 的筛选遍历。`limit` 只计入实际输出，达到上限后停止下钻；
/// 目录条目始终输出 (TS 0a7417f)，`type=file` 时不输出但仍下钻。
pub async fn list_files_meta_filtered(
    root: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    relative_path: Option<&str>,
    options: MetaListOptions,
) -> AppResult<Vec<FileEntry>> {
    let list_dir = resolve_subdir(root, relative_path)?;
    // realpath 级根边界 (对齐 TS e822516 escapesRoot): relativePath 指进目录符号链接
    // (如 link -> /etc 后 relativePath=link) 时列表起点实际位于目标根之外, 按非法
    // 路径拒绝——与字面 `..` 穿越同一报错口径; 缺失路径不在此层报错 (先于存在性检查,
    // 与 TS 顺序一致)。
    ensure_resolved_list_dir(root, &list_dir, relative_path).await?;
    if !crate::service::fs_util::path_exists(&list_dir).await? {
        // 目录不存在 → 空数组 (handler 层也会先判存在, 这里是防御)
        return Ok(Vec::new());
    }
    let meta = fs::metadata(&list_dir).await?;
    if !meta.is_dir() {
        return Err(AppError::validation("relativePath must be a directory"));
    }
    let mut files = Vec::new();
    let start = Instant::now();
    if !options.reached_limit(0) {
        if options.recursive {
            traverse_meta(root, &list_dir, config, proxy_path, options, &mut files).await?;
        } else {
            list_directory_level(root, &list_dir, config, proxy_path, options, &mut files).await?;
        }
    }
    tracing::info!(
        op = "list_files_meta",
        elapsed_ms = start.elapsed().as_millis(),
        file_count = files.len(),
        recursive = options.recursive,
        file_type = %options.file_type,
        limit = ?options.limit,
        "file listing completed"
    );
    Ok(files)
}

/// computer 列表使用宿主路径语义：POSIX 的反斜杠属于文件名，仅 Windows
/// 将其归一为路径分隔符（统一经 [`crate::path_safety::host_relative_to_wire`]）。
/// project 内容遍历保留既有的 `make_relative_path` 口径。
fn make_list_relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    crate::path_safety::host_relative_to_wire(&relative)
}

/// 单层/受限层级遍历 (对齐 nuwax computer `listDirectoryLevel`): `levels_left=1`
/// 仅列 `dir` 下一层; N>1 时目录向下再展开 N-1 级 (TS 1.5.4 `depth`, DFS——
/// 目录条目后紧跟其子项)。
/// 空目录 (无任何可见条目) 不会产生节点 (TS listDirectoryLevel 同样不返回空目录自身)。
/// type=file 时目录条目不输出但**仍下钻** (深层文件取得到, 与 search 的 type 口径
/// 一致); 达 limit 后不再下钻; 单个子目录不可读 → WARN 跳过其子树不拖垮请求。
async fn list_directory_level(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    options: MetaListOptions,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let items = read_filtered_entries(dir, config, DirectoryExcludePolicy::PlatformAware).await?;
    for (_name, path, is_dir, is_link) in items {
        if options.reached_limit(out.len()) {
            break;
        }
        if !is_dir && options.file_type == MetaListType::Dir {
            continue; // type=dir 下文件不输出; type=file 下目录仍需下钻 (下方统一处理)
        }
        let relative = make_list_relative_path(root, &path);
        if !is_safe_list_link(root, &path, is_link).await {
            continue;
        }
        if is_dir {
            if options.file_type != MetaListType::File {
                out.push(FileEntry {
                    name: relative.clone(),
                    is_dir: true,
                    binary: None,
                    size_exceeded: None,
                    contents: None,
                    file_proxy_url: None,
                    is_link: Some(is_link),
                });
            }
            // 层级未用尽且未达 limit 时下钻; 子目录读失败保留目录条目、跳过子树
            if options.levels_left > 1 && !options.reached_limit(out.len()) {
                let child_options = MetaListOptions {
                    levels_left: options.levels_left - 1,
                    ..options
                };
                if let Err(error) = Box::pin(list_directory_level(
                    root,
                    &path,
                    config,
                    proxy_path,
                    child_options,
                    out,
                ))
                .await
                {
                    tracing::warn!(error = %error, subtree = %relative, "depth descent failed, skip subtree");
                }
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

/// 递归遍历 (对齐 nuwax computer `traverseDirectory`, TS 0a7417f): **目录条目始终
/// 输出** (非空目录不再只铺开子项, 与 `list_directory_level` 口径一致), DFS——
/// 目录条目后紧跟其子项; `type=file` 时目录条目不输出但仍下钻 (深层文件取得到);
/// 达 limit 后提前终止; 单个子目录不可读/扫描中被删除 → WARN 跳过其子树,
/// 保留目录条目, 不拖垮整个请求。
async fn traverse_meta(
    root: &Path,
    dir: &Path,
    config: &Config,
    proxy_path: Option<&str>,
    options: MetaListOptions,
    out: &mut Vec<FileEntry>,
) -> AppResult<()> {
    let items = read_filtered_entries(dir, config, DirectoryExcludePolicy::PlatformAware).await?;
    for (_name, path, is_dir, is_link) in items {
        if options.reached_limit(out.len()) {
            break;
        }
        let relative = make_list_relative_path(root, &path);
        if !is_safe_list_link(root, &path, is_link).await {
            continue;
        }
        if is_dir {
            if options.file_type != MetaListType::File && !options.reached_limit(out.len()) {
                out.push(FileEntry {
                    name: relative.clone(),
                    is_dir: true,
                    binary: None,
                    size_exceeded: None,
                    contents: None,
                    file_proxy_url: None,
                    is_link: Some(is_link),
                });
            }
            if let Err(error) =
                Box::pin(traverse_meta(root, &path, config, proxy_path, options, out)).await
            {
                tracing::warn!(error = %error, subtree = %relative, "depth descent failed, skip subtree");
            }
        } else if options.file_type != MetaListType::Dir {
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

/// 列表可展示根目录内的链接，但不暴露指向所选根之外的链接目标。
/// 列表条目的链接安全判定 (对齐 TS 0a7417f `isHiddenSymlink`): 链接目标 realpath
/// 解析后必须落在根内; **悬空链接 (目标不存在, realpath 失败) 同样不可见**——
/// 断链打开必 404, 混进列表只会产生打不开的条目; 根 realpath 失败也隐藏。
/// 仅用于列表条目过滤; resolve-file 等读取链路仍走 `ensure_resolved_within`
/// 的 fail-open 口径 (realpath 失败按未越界交常规流程, 对齐 escapesRoot)。
async fn is_safe_list_link(root: &Path, path: &Path, is_link: bool) -> bool {
    if !is_link {
        return true;
    }
    match fs::canonicalize(root).await {
        // 使用 readdir 返回的真实路径，不能从展示字符串重建宿主路径。
        Ok(resolved_root) => match fs::canonicalize(path).await {
            Ok(resolved) => resolved.starts_with(&resolved_root),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

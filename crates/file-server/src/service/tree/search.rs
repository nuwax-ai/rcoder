//! search-files: 无索引有界实时搜索 (对齐 TS `searchFiles`)。
//!
//! 遍历引擎用 [`dua_core`] 的并行 work-stealing 线程池 (取代手写 BFS):
//! - `std::fs::DirEntry::file_type()` 直接读 `d_type`, 避免 tokio 的 lstat fallback;
//! - 多线程并行读目录, 大目录树延迟显著下降;
//! - `descend` 谓词在遍历层剪枝排除目录, 不展开其子项;
//! - 同步迭代器, 通过 `tokio::task::spawn_blocking` 在阻塞线程跑, 不占用 async runtime。
//!
//! 从 `tree` 模块拆出: 搜索逻辑体量大, 独立成模块便于维护。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::error::{AppError, AppResult};

use super::{FileEntry, build_file_proxy_url, resolve_subdir};

/// 并行遍历线程上限: 取 CPU 并行度但封顶, 避免小目录过度开线程的调度开销。
const MAX_SEARCH_THREADS: usize = 8;

/// 搜索结果 (对齐 TS `{files, truncated, visited}`)。
#[derive(Serialize)]
pub struct SearchResult {
    pub files: Vec<FileEntry>,
    pub truncated: bool,
    pub visited: usize,
}

/// [`search_files`] 入参。把搜索目标与边界约束聚合, 避免 8 参数长签名。
///
/// - `kw`/`limit`/`max_visit`/`timeout_ms` 的非空与正整数校验由调用方 (handler) 完成。
/// - 返回的 `file_proxy_url` 不含 customTargetDir 后缀, 由 handler 统一追加。
pub struct SearchParams<'a> {
    /// 搜索根目录 (默认工作区或 customTargetDir)。
    pub root: &'a Path,
    pub config: &'a Config,
    /// fileProxyUrl 前缀; `None` 则不生成 fileProxyUrl。
    pub proxy_path: Option<&'a str>,
    /// 关键字 (文件名或相对路径, 大小写不敏感子串匹配)。
    pub kw: &'a str,
    /// 相对 `root` 的搜索起点子目录; `None`/空 → `root` 本身。
    pub relative_path: Option<&'a str>,
    /// 命中条数上限。
    pub limit: usize,
    /// 访问条目数硬上限 (含未命中)。
    pub max_visit: usize,
    /// 超时毫秒数。
    pub timeout_ms: u64,
}

/// 无索引有界实时搜索 (并行遍历): 返回命中项, 受 `limit`/`max_visit`/`timeout_ms` 三重边界约束。
///
/// 对齐 TS `searchFiles`:
/// - 关键字匹配文件名或相对路径 (大小写不敏感, 子串包含)。
/// - 排除规则同遍历 (隐藏文件除 .gitignore / traverse_exclude_dirs / content_traverse_exclude_files)。
/// - 硬停止: `visited >= max_visit` 或超时; 命中达 `limit` 标记 truncated。
/// - `truncated` 综合判定: 迭代器提前终止 / visited 达上限 / 超时。
///
/// 遍历由 [`dua_core::walk`] 的 work-stealing 线程池驱动 (ParentFirst 顺序), 通过
/// [`tokio::task::spawn_blocking`] 在阻塞线程执行, 不占用 async runtime。
pub async fn search_files(params: SearchParams<'_>) -> AppResult<SearchResult> {
    let start = Instant::now();
    let SearchParams {
        root,
        config,
        proxy_path,
        kw,
        relative_path,
        limit,
        max_visit,
        timeout_ms,
    } = params;
    let kw_lower = kw.to_lowercase();
    let timeout = Duration::from_millis(timeout_ms);

    let search_root_abs = resolve_subdir(root, relative_path)?;
    if !crate::service::fs_util::path_exists(&search_root_abs).await? {
        return Ok(SearchResult {
            files: Vec::new(),
            truncated: false,
            visited: 0,
        });
    }
    let sr_meta = tokio::fs::metadata(&search_root_abs).await?;
    if !sr_meta.is_dir() {
        return Err(AppError::validation("relativePath must be a directory"));
    }

    // 预处理排除规则为 HashSet (O(1) 查找), 聚合进 BlockingCtx 跨 spawn_blocking 边界。
    let exclude_dirs = config
        .traverse_exclude_dirs
        .iter()
        .cloned()
        .collect::<HashSet<String>>();
    let exclude_files = config
        .content_traverse_exclude_files
        .iter()
        .cloned()
        .collect::<HashSet<String>>();

    let ctx = BlockingCtx {
        root: root.to_path_buf(),
        search_root: search_root_abs,
        proxy_path: proxy_path.map(str::to_string),
        exclude_dirs,
        exclude_files,
        kw_lower,
        limit,
        max_visit,
        timeout,
    };

    // 同步遍历放在 spawn_blocking 里: 目录遍历是 syscall 密集型, 在专用线程跑
    // 既拿到 std::fs 的零 syscall d_type, 又不阻塞 async runtime。
    let (files, truncated, visited) = tokio::task::spawn_blocking(move || search_blocking(&ctx))
        .await
        .map_err(|e| AppError::system(format!("search join failed: {e}")))?;

    tracing::info!(
        kw,
        match_count = files.len(),
        visited,
        truncated,
        elapsed_ms = start.elapsed().as_millis(),
        "file search completed"
    );

    Ok(SearchResult {
        files,
        truncated,
        visited,
    })
}

/// [`search_blocking`] 的上下文: 聚合所有搜索参数, 跨 `spawn_blocking` 边界所有权转移。
struct BlockingCtx {
    /// 工作区根 (计算相对路径的基准)。
    root: PathBuf,
    /// 实际遍历起点 (root 本身或其子目录)。
    search_root: PathBuf,
    proxy_path: Option<String>,
    exclude_dirs: HashSet<String>,
    exclude_files: HashSet<String>,
    kw_lower: String,
    limit: usize,
    max_visit: usize,
    timeout: Duration,
}

/// 阻塞线程内的同步遍历 + 关键字过滤。
///
/// 返回 `(matches, truncated, visited)`。
fn search_blocking(ctx: &BlockingCtx) -> (Vec<FileEntry>, bool, usize) {
    let start = Instant::now();

    // descend: 排除目录不展开子项 (但仍会被产出 → 消费时跳过)。
    // 闭包要求 'static + Send + Sync, 故 clone 一份所有权 (几十项, 成本可忽略)。
    let exclude_dirs_for_descend = ctx.exclude_dirs.clone();
    let descend = move |entry: &dua_core::Entry| {
        if entry.file_type.is_dir() {
            let name = entry.file_name.to_string_lossy();
            !exclude_dirs_for_descend.contains(&*name)
        } else {
            true
        }
    };

    // 线程数: 取可用并行度, 上限 MAX_SEARCH_THREADS (小目录避免过度开线程的调度开销)。
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_SEARCH_THREADS);

    let walker = dua_core::walk(
        &ctx.search_root,
        threads,
        dua_core::Order::ParentFirst,
        descend,
    );

    let proxy = ctx.proxy_path.as_deref();
    let mut matches: Vec<FileEntry> = Vec::new();
    let mut visited = 0usize;
    let mut truncated = false;

    for item in walker {
        // 三重边界检查 (超时 / 访问数 / 命中数), 任一超限 → 标记 truncated 并终止
        if start.elapsed() >= ctx.timeout || visited >= ctx.max_visit || matches.len() >= ctx.limit
        {
            truncated = true;
            break;
        }

        let entry = match item {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "search entry error");
                continue;
            }
        };

        // depth==0 是搜索根本身, 跳过
        if entry.depth == 0 {
            continue;
        }

        // Cow 借用, 不做 OsString→String 堆分配 (仅命中项最终进入结果)
        let name = entry.file_name.to_string_lossy();

        // 隐藏文件 (除 .gitignore) 跳过 (对齐 TS isExcludedSearchEntry)
        if name.starts_with('.') && name != super::KEEP_HIDDEN_FILE {
            continue;
        }
        // 排除文件跳过
        if ctx.exclude_files.contains(&*name) {
            continue;
        }

        let is_dir = entry.file_type.is_dir();
        let is_link = entry.file_type.is_symlink();

        // 排除目录: descend 已产出但不应出现在结果 (对齐 TS 完全跳过语义)
        if is_dir && ctx.exclude_dirs.contains(&*name) {
            continue;
        }

        visited += 1;

        // 相对 root 的 POSIX 路径 (搜索结果 name 字段)
        let rel = make_relative_posix(&ctx.root, &entry);

        // 匹配只查 rel: name 是 rel 的尾段子串, contains(name) ⊆ contains(rel),
        // TS 原版的两查在结果上等价于单查 rel。零分配大小写不敏感匹配
        // (ASCII 快速路径; 非 ASCII fallback Unicode to_lowercase 保持语义)。
        if !contains_ignore_case(&rel, &ctx.kw_lower) {
            continue;
        }

        if is_dir {
            matches.push(FileEntry {
                name: rel,
                is_dir: true,
                binary: None,
                size_exceeded: None,
                contents: None,
                file_proxy_url: None,
                is_link: Some(is_link),
            });
        } else {
            matches.push(FileEntry {
                name: rel.clone(),
                is_dir: false,
                binary: None,
                size_exceeded: None,
                contents: None,
                file_proxy_url: build_file_proxy_url(proxy, &rel),
                is_link: Some(is_link),
            });
        }
    }
    // 迭代器 drop → dua_core Pool::drop → stop + wake_workers + join (优雅终止)

    // 排序: 目录在前 + 名字大小写不敏感 (对齐 TS localeCompare)。
    // cached_key: 每元素小写一次, 取代比较器每次比较的两次 to_lowercase 分配。
    matches.sort_by_cached_key(|e| (std::cmp::Reverse(e.is_dir), e.name.to_lowercase()));

    (matches, truncated, visited)
}

/// 计算相对 `root` 的 POSIX 风格路径。
///
/// dua_core::Entry 的 `parent_path` + `file_name` 组成完整绝对路径, 去掉 root 前缀
/// 得到相对路径。搜索根可能为 root 的子目录 (relative_path), 但条目路径是绝对的,
/// 故前缀裁剪始终能得到正确的相对工作区路径。
///
/// 性能: 每个访问条目都会调用 (命中前), 用"parent 切片 + format 拼接"一次分配,
/// 取代 Path::join + strip_prefix + replace 的三次分配; Windows 分隔符条件替换。
fn make_relative_posix(root: &Path, entry: &dua_core::Entry) -> String {
    let parent = entry.parent_path.to_string_lossy();
    let root_str = root.to_string_lossy();
    let root_trimmed = root_str.trim_end_matches('/');
    // parent==root（根级条目）→ rel 空; parent=root/sub → "sub"; 非 root 前缀
    // （customTargetDir 等场景）→ 退化为绝对路径去前导斜杠。
    let rel = match parent.strip_prefix(root_trimmed) {
        Some(rest) => rest.trim_start_matches('/'),
        None => parent.trim_start_matches('/'),
    };
    let name = entry.file_name.to_string_lossy();
    let joined = if rel.is_empty() {
        name.into_owned()
    } else {
        format!("{rel}/{name}")
    };
    if joined.contains('\\') {
        joined.replace('\\', "/")
    } else {
        joined
    }
}

/// 大小写不敏感子串匹配 (kw 须已转小写, 由调用方保证; 空串恒 false)。
///
/// 性能: haystack/kw 均为 ASCII 时走零分配滑窗 (逐字节 ASCII 折叠比较);
/// 任一非 ASCII 时 fallback `to_lowercase()` (Unicode 语义, 对齐 TS toLowerCase)。
/// 搜索热路径每个访问条目调用一次, 分配开销是大目录场景的主要浪费源。
fn contains_ignore_case(haystack: &str, kw_lower: &str) -> bool {
    if kw_lower.is_empty() {
        return false;
    }
    if kw_lower.is_ascii() && haystack.is_ascii() {
        let h = haystack.as_bytes();
        let n = kw_lower.as_bytes();
        if h.len() < n.len() {
            return false;
        }
        'outer: for i in 0..=h.len() - n.len() {
            for j in 0..n.len() {
                if h[i + j].to_ascii_lowercase() != n[j] {
                    continue 'outer;
                }
            }
            return true;
        }
        false
    } else {
        haystack.to_lowercase().contains(kw_lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn default_test_config() -> Config {
        Config::default()
    }

    /// 构造测试目录结构 (与 mod.rs tests 同构):
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
        tokio::fs::create_dir_all(root.join("sub").join("nested"))
            .await
            .unwrap();
        tokio::fs::write(root.join("a.txt"), "a").await.unwrap();
        tokio::fs::write(root.join("b.md"), "b").await.unwrap();
        tokio::fs::write(root.join("sub").join("c.txt"), "c")
            .await
            .unwrap();
        tokio::fs::write(root.join("sub").join("d.log"), "d")
            .await
            .unwrap();
        tokio::fs::write(root.join("sub").join("nested").join("e.txt"), "e")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_files_matches_by_filename_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // 关键字 ".txt" 应命中所有 .txt 文件 (按文件名匹配)
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: ".txt",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub/c.txt"));
        assert!(names.contains(&"sub/nested/e.txt"));
        // .md / .log 不命中
        assert!(!names.contains(&"b.md"));
        assert!(!names.contains(&"sub/d.log"));
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn search_files_keyword_in_path_matches() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // 关键字 "nested" → 命中 nested 目录 + 其下文件 (相对路径含 "nested")
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "nested",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"sub/nested")); // 目录节点
        assert!(names.contains(&"sub/nested/e.txt")); // 路径含 nested
    }

    #[tokio::test]
    async fn search_files_limit_truncates() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // limit=1, 多个 .txt 命中 → truncated=true
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: ".txt",
            relative_path: None,
            limit: 1,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        assert_eq!(r.files.len(), 1);
        assert!(r.truncated);
    }

    #[tokio::test]
    async fn search_files_respects_exclude_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // node_modules 默认在 traverse_exclude_dirs, 应被跳过
        tokio::fs::create_dir_all(tmp.path().join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("node_modules").join("target.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("keep.txt"), "x")
            .await
            .unwrap();
        let cfg = default_test_config();
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "target",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        // node_modules/target.txt 不应命中
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    #[tokio::test]
    async fn search_files_excluded_dir_itself_not_yielded() {
        // 边界: 即使关键字匹配排除目录名, 排除目录本身也不应出现在结果 (对齐 TS 完全跳过语义)。
        // dua_core descend 拒绝的目录仍会产出条目 → 消费循环必须显式跳过。
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("node_modules").join("x.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("keep.txt"), "x")
            .await
            .unwrap();
        let cfg = default_test_config();
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "node", // 匹配排除目录名本身
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        // node_modules 目录本身 + 其下文件都不应出现
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "excluded dir should not be yielded, got {names:?}"
        );
    }

    #[tokio::test]
    async fn search_files_no_match_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "zzz_no_such",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        assert!(r.files.is_empty());
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn search_files_relative_path_scopes_search() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let cfg = default_test_config();
        // relative_path="sub" → 仅搜索 sub 子树
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: ".txt",
            relative_path: Some("sub"),
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"sub/c.txt"));
        assert!(names.contains(&"sub/nested/e.txt"));
        // 根层 a.txt 不在搜索范围
        assert!(!names.contains(&"a.txt"));
    }

    #[test]
    fn contains_ignore_case_is_case_insensitive_substring() {
        // kw 须为小写 (调用方保证); 大小写不敏感子串匹配 (只查 rel, 与旧两查等价)
        assert!(contains_ignore_case("src/Foo.ts", "foo"));
        assert!(contains_ignore_case("src/foo.ts", "foo"));
        assert!(contains_ignore_case("src/x.rs", "src/x")); // 路径段命中
        assert!(!contains_ignore_case("a.txt", "b"));
        assert!(!contains_ignore_case("a.txt", "")); // 空 kw → false
    }

    #[test]
    fn contains_ignore_case_unicode_fallback() {
        // 非 ASCII 走 Unicode to_lowercase fallback, 语义与 ASCII 快速路径一致
        assert!(contains_ignore_case("数据/报表.csv", "报表"));
        assert!(contains_ignore_case("Ähnlich.txt", "ähnlich")); // Unicode 折叠
        assert!(!contains_ignore_case("数据/x.csv", "报表a"));
    }
}

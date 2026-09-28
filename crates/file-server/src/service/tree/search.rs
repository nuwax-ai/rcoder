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

/// 搜索遍历的目录黑名单: 仅纯噪音目录 (VCS 元数据/依赖/解释器缓存)。
/// 刻意放行产品运行时目录 (.claude/.agents/.codex/.opencode/.local-deploy 等) ——
/// 其下的技能 (SKILL.md) 与配置文件正是搜索要找的目标; 与浏览列表的隐藏口径
/// (listDirectoryLevel 对点开头条目的整体隐藏) 解耦, 各管各的。
/// 与 env TRAVERSE_EXCLUDE_DIRS 合并生效: 配置可追加排除项, 不可放行内置噪音项。
/// (对齐 TS SEARCH_NOISE_DIR_NAMES, nuwax commit 9f636bf)
const SEARCH_NOISE_DIR_NAMES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "virtualenv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".cache",
];

/// 搜索遍历的文件黑名单: 系统生成的噪音文件 (点开头条目不再整体排除)。
/// (对齐 TS SEARCH_NOISE_FILE_NAMES, nuwax commit 9f636bf)
const SEARCH_NOISE_FILE_NAMES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

/// 条目名是否命中搜索噪音文件黑名单 (不区分条目类型, 目录撞名同样整体排除)。
fn is_search_noise_file(name: &str) -> bool {
    SEARCH_NOISE_FILE_NAMES.contains(&name)
}

/// 目录名是否命中搜索噪音目录黑名单。
fn is_search_noise_dir(name: &str) -> bool {
    SEARCH_NOISE_DIR_NAMES.contains(&name)
}

/// search-files 命中类型过滤 (TS 104d285 `type` 参数归一后的两种有效形态):
/// File=仅文件命中 / Dir=仅目录命中; None=全部 (空/all/非法, 保持既有行为)。
/// 仅过滤命中输出, 不影响遍历下钻——type=file 时目录仍递归, 深层文件才搜得到。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKindFilter {
    File,
    Dir,
}

impl SearchKindFilter {
    /// 归一化解析 (口径同 get-file-list 的 MetaListType): trim + 小写后
    /// `file`→File、`dir`/`directory`→Dir; 空/all/非法→None (不过滤, 不报错)。
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        let raw = raw?.trim().to_lowercase();
        match super::MetaListType::parse_normalized(&raw) {
            Some(super::MetaListType::File) => Some(Self::File),
            Some(super::MetaListType::Dir) => Some(Self::Dir),
            _ => None,
        }
    }

    /// 条目类别是否允许进入命中输出 (对齐 TS allowsMatch)。
    fn allows(self, is_dir: bool) -> bool {
        match self {
            Self::File => !is_dir,
            Self::Dir => is_dir,
        }
    }
}

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
    /// 命中类型过滤 (TS 104d285 `type`); None=全部。仅过滤命中输出, 不影响遍历。
    pub kind_filter: Option<SearchKindFilter>,
}

/// 无索引有界实时搜索 (并行遍历): 返回命中项, 受 `limit`/`max_visit`/`timeout_ms` 三重边界约束。
///
/// 对齐 TS `searchFiles` (排除口径 nuwax commit 9f636bf 起与浏览列表解耦):
/// - 关键字匹配文件名或相对路径 (大小写不敏感, 子串包含)。
/// - 排除 = 噪音黑名单 (SEARCH_NOISE_DIR_NAMES / SEARCH_NOISE_FILE_NAMES,
///   配置不可放行) + traverse_exclude_dirs / content_traverse_exclude_files;
///   点开头条目不再整体排除 —— .claude 等产品运行时目录下的技能与配置可被搜到。
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
        kind_filter,
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
        kind_filter,
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
    /// 命中类型过滤; None=全部 (TS 104d285)。
    kind_filter: Option<SearchKindFilter>,
}

/// 阻塞线程内的同步遍历 + 关键字过滤。
///
/// 返回 `(matches, truncated, visited)`。
fn search_blocking(ctx: &BlockingCtx) -> (Vec<FileEntry>, bool, usize) {
    let start = Instant::now();

    // descend: 排除目录不展开子项 (但仍会被产出 → 消费时跳过)。
    // 排除 = 噪音目录 ∪ 配置排除目录 ∪ 撞上噪音文件/配置排除文件名的目录 ——
    // TS 侧这些目录不入队 (isExcludedSearchEntry 先于 childDirs.push), 子树整体不可见。
    // 闭包要求 'static + Send + Sync, 故 clone 一份所有权 (几十项, 成本可忽略)。
    let exclude_dirs_for_descend = ctx.exclude_dirs.clone();
    let exclude_files_for_descend = ctx.exclude_files.clone();
    let descend = move |entry: &dua_core::Entry| {
        if entry.file_type.is_dir() {
            let name = entry.file_name.to_string_lossy();
            !(exclude_dirs_for_descend.contains(&*name)
                || is_search_noise_dir(&name)
                || exclude_files_for_descend.contains(&*name)
                || is_search_noise_file(&name))
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
        // dua-core 3.3 新增的平台元数据开关（macOS APFS clone）默认关闭，
        // 与 3.1 行为一致——search-files 只消费路径与文件类型
        dua_core::Options::default(),
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

        // 噪音文件 (系统生成) 跳过; 不区分条目类型 —— 目录撞名同样整体排除
        // (对齐 TS SEARCH_NOISE_FILE_NAMES 未按 isDirectory 门控)
        if is_search_noise_file(&name) {
            continue;
        }
        // 排除文件跳过 (同样不区分类型, 对齐 TS excludeFiles 判定)
        if ctx.exclude_files.contains(&*name) {
            continue;
        }

        let is_dir = entry.file_type.is_dir();
        let is_link = entry.file_type.is_symlink();

        // 噪音目录 / 排除目录: descend 已拒绝但仍会产出条目 → 显式跳过
        // (对齐 TS 完全跳过语义; 内置噪音项不因配置缺失而放行)
        if is_dir && (is_search_noise_dir(&name) || ctx.exclude_dirs.contains(&*name)) {
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

        // 类型过滤仅作用于命中输出 (TS 104d285 allowsMatch): 类别不符的条目
        // 不进结果也不占用 limit; visited/遍历下钻不受影响; 无过滤(None)全放行。
        if ctx.kind_filter.is_some_and(|f| !f.allows(is_dir)) {
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
            kind_filter: None,
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
            kind_filter: None,
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
            kind_filter: None,
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
            kind_filter: None,
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
            kind_filter: None,
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
    async fn search_files_finds_entries_in_hidden_product_dirs() {
        // TS 9f636bf: 搜索放行产品运行时目录 (.claude 等), 其下的 SKILL.md 与配置
        // 正是搜索目标; 点开头文件 (.env.example / .gitignore) 不再整体排除。
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join(".claude").join("skills").join("deploy");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(skill_dir.join("SKILL.md"), "skill")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join(".env.example"), "env")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join(".gitignore"), "ignore")
            .await
            .unwrap();
        let cfg = default_test_config();
        for (kw, expected) in [
            ("skill", ".claude/skills/deploy/SKILL.md"),
            ("env.example", ".env.example"),
            ("gitignore", ".gitignore"),
        ] {
            let r = search_files(SearchParams {
                root: tmp.path(),
                config: &cfg,
                proxy_path: None,
                kw,
                relative_path: None,
                limit: 100,
                max_visit: 1000,
                timeout_ms: 5000,
                kind_filter: None,
            })
            .await
            .unwrap();
            let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
            assert!(
                names.contains(&expected),
                "kw={kw} 应命中 {expected}, 实际: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn search_files_skips_noise_files_and_dirs() {
        // TS 9f636bf: 噪音黑名单 —— .DS_Store 等噪音文件、.venv/__pycache__/.git 等
        // 噪音目录 (含整个子树) 即便关键字命中也不可见。.venv/__pycache__/.cache
        // 不在默认 traverse_exclude_dirs, 只有噪音表能挡住 —— 本测试即锁定该表。
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join(".DS_Store"), "junk")
            .await
            .unwrap();
        for noise_dir in [".venv", "__pycache__", ".git"] {
            tokio::fs::create_dir_all(tmp.path().join(noise_dir))
                .await
                .unwrap();
            tokio::fs::write(tmp.path().join(noise_dir).join("target.txt"), "x")
                .await
                .unwrap();
        }
        tokio::fs::write(tmp.path().join("keep-target.txt"), "x")
            .await
            .unwrap();
        let cfg = default_test_config();
        // kw=target: 正常文件命中 + 噪音条目全部不可见
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "target",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
            kind_filter: None,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"keep-target.txt"),
            "正常文件应命中: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".DS_Store")
                || n.starts_with(".venv")
                || n.starts_with("__pycache__")
                || n.starts_with(".git")),
            "噪音条目不应出现: {names:?}"
        );
        // kw=store: 唯一含 "store" 的条目是噪音文件 .DS_Store → 结果必须为空
        let r = search_files(SearchParams {
            root: tmp.path(),
            config: &cfg,
            proxy_path: None,
            kw: "store",
            relative_path: None,
            limit: 100,
            max_visit: 1000,
            timeout_ms: 5000,
            kind_filter: None,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.is_empty(),
            "噪音文件 .DS_Store 即便关键字命中也不应出现: {names:?}"
        );
    }

    #[tokio::test]
    async fn search_files_dir_named_like_excluded_file_hides_subtree() {
        // 对齐 TS: 噪音文件/排除文件判定不区分条目类型 —— 目录撞名 (如默认
        // content_traverse_exclude_files 中的 CLAUDE.md) 同样整体排除, 子树不可见。
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("CLAUDE.md"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("CLAUDE.md").join("inner-target.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("keep-target.txt"), "x")
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
            kind_filter: None,
        })
        .await
        .unwrap();
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("CLAUDE.md")),
            "撞排除文件名的目录子树不应可见: {names:?}"
        );
        assert!(names.contains(&"keep-target.txt"));
    }

    #[tokio::test]
    async fn search_files_kind_filter_selects_output_type() {
        // TS 104d285: type=file/dir 只过滤命中输出——kw 同时命中目录节点与
        // 文件时按类别筛; type=file 时目录仍下钻, 深层文件必须能搜到。
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("sub").join("nested"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("sub").join("nested").join("sub.txt"), "x")
            .await
            .unwrap();
        let cfg = default_test_config();
        let run = |kind_filter| {
            search_files(SearchParams {
                root: tmp.path(),
                config: &cfg,
                proxy_path: None,
                kw: "sub",
                relative_path: None,
                limit: 100,
                max_visit: 1000,
                timeout_ms: 5000,
                kind_filter,
            })
        };
        let all = run(None).await.unwrap();
        let names: Vec<&str> = all.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"sub"), "默认含目录节点: {names:?}");
        assert!(
            names.contains(&"sub/nested/sub.txt"),
            "默认含文件: {names:?}"
        );

        let files_only = run(Some(SearchKindFilter::File)).await.unwrap();
        assert!(
            files_only.files.iter().all(|f| !f.is_dir),
            "type=file 不应有目录命中: {:?}",
            files_only
                .files
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<_>>()
        );
        let names: Vec<&str> = files_only.files.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"sub/nested/sub.txt"),
            "type=file 深层文件必须能搜到 (目录仍下钻): {names:?}"
        );

        let dirs_only = run(Some(SearchKindFilter::Dir)).await.unwrap();
        let names: Vec<&str> = dirs_only.files.iter().map(|f| f.name.as_str()).collect();
        // kw="sub" 同时命中 sub 与 sub/nested 两个目录节点 (rel 含 "sub")
        assert_eq!(
            names,
            vec!["sub", "sub/nested"],
            "type=dir 仅目录节点: {names:?}"
        );
    }

    #[test]
    fn search_kind_filter_parse_matches_ts_normalization() {
        use SearchKindFilter::{Dir, File};
        assert_eq!(SearchKindFilter::parse(None), None);
        assert_eq!(SearchKindFilter::parse(Some("")), None);
        assert_eq!(SearchKindFilter::parse(Some("   ")), None);
        assert_eq!(SearchKindFilter::parse(Some("all")), None);
        assert_eq!(SearchKindFilter::parse(Some("bogus")), None);
        assert_eq!(SearchKindFilter::parse(Some("file")), Some(File));
        assert_eq!(SearchKindFilter::parse(Some("FILE")), Some(File));
        assert_eq!(SearchKindFilter::parse(Some(" File ")), Some(File));
        assert_eq!(SearchKindFilter::parse(Some("dir")), Some(Dir));
        assert_eq!(SearchKindFilter::parse(Some("directory")), Some(Dir));
        assert_eq!(SearchKindFilter::parse(Some(" Directory ")), Some(Dir));
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
            kind_filter: None,
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
            kind_filter: None,
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

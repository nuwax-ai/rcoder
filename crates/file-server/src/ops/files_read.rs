//! 文件读类共享实现：get-file-list / resolve-file / search-files / get-file-meta。
//!
//! 壳与 handler 层测试在 handlers/computer/files_read.rs；参数经
//! `FileListParams` / `SearchFilesParams` 借用结构传入，定位（computer 树或
//! userapp 开发卷）由各域壳层完成。

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde_json::json;

use crate::AppState;
use crate::config::Config;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{
    ComputerFileEntry, FileListResult, FileMetaEntryResult, FileMetaResult, ResolveFileResult,
    SearchFilesResult,
};
use crate::service::{code as code_service, tree};

#[derive(Clone, Copy)]
pub struct FileListParams<'a> {
    pub proxy_path: Option<&'a str>,
    pub relative_path: Option<&'a str>,
    /// 原始 recursive 串: 缺省/非 "false" 均按递归 (对齐 TS)。
    pub recursive: Option<&'a str>,
    /// `type`: all/file/dir (directory 为 dir 别名)，缺省 all。
    pub file_type: Option<&'a str>,
    /// `limit`: 非负整数，缺省不限；上限由 usize 表示范围决定。
    pub limit: Option<&'a str>,
    /// 原始 customTargetDir 串: 仅用于 fileProxyUrl 后缀 (定位语义由壳层消化)。
    pub custom_target_dir: Option<&'a str>,
}

pub struct FileListOutcome {
    pub files: Vec<tree::FileEntry>,
    pub recursive: bool,
    pub file_type: tree::MetaListType,
    pub limit: Option<usize>,
}

fn parse_file_list_type(raw: Option<&str>) -> Result<tree::MetaListType, AppError> {
    let raw = raw.unwrap_or_default();
    tree::MetaListType::parse_normalized(&raw.trim().to_ascii_lowercase()).ok_or_else(|| {
        // 对齐 TS 契约: 文案与 details.value (回显原始输入) 一致，便于调用方定位。
        AppError::validation_with(
            "type 仅支持 file/dir/all",
            json!({ "field": "type", "value": raw }),
        )
    })
}

fn parse_file_list_limit(raw: Option<&str>) -> Result<Option<usize>, AppError> {
    match raw {
        None | Some("") => Ok(None),
        // TS Number("  ") 为 0；空串本身仍表示未指定。
        Some(value) if value.trim().is_empty() => Ok(Some(0)),
        Some(value) => value.trim().parse::<usize>().map(Some).map_err(|_| {
            AppError::validation_with(
                "limit 必须为非负整数",
                json!({ "field": "limit", "value": value }),
            )
        }),
    }
}

#[cfg(test)]
mod file_list_options_tests {
    use super::{parse_file_list_limit, parse_file_list_type};
    use crate::error::AppError;
    use crate::service::tree::MetaListType;

    #[test]
    fn options_reject_invalid_values_and_accept_boundary_values() {
        assert_eq!(
            parse_file_list_type(Some(" DIRECTORY ")).unwrap(),
            MetaListType::Dir
        );
        assert!(parse_file_list_type(Some("other")).is_err());
        assert_eq!(parse_file_list_limit(None).unwrap(), None);
        assert_eq!(parse_file_list_limit(Some("0")).unwrap(), Some(0));
        assert_eq!(
            parse_file_list_limit(Some(&usize::MAX.to_string())).unwrap(),
            Some(usize::MAX)
        );
        assert!(parse_file_list_limit(Some("-1")).is_err());
        assert!(parse_file_list_limit(Some("1.5")).is_err());
    }

    #[test]
    fn invalid_options_echo_raw_input_in_details() {
        // 对齐 TS ValidationError 契约: 中文文案 + details 回显原始输入值。
        let AppError::Validation(message, details) =
            parse_file_list_type(Some("Invalid")).unwrap_err()
        else {
            panic!("invalid type must produce a validation error");
        };
        assert_eq!(message, "type 仅支持 file/dir/all");
        let details = details.expect("validation details");
        assert_eq!(details["field"], "type");
        assert_eq!(details["value"], "Invalid");

        let AppError::Validation(message, details) = parse_file_list_limit(Some("-3")).unwrap_err()
        else {
            panic!("invalid limit must produce a validation error");
        };
        assert_eq!(message, "limit 必须为非负整数");
        let details = details.expect("validation details");
        assert_eq!(details["field"], "limit");
        assert_eq!(details["value"], "-3");
    }
}

/// get-file-list 的 workspace 无关核心 (computer / userapp 域共用;
/// 定位由各域壳层完成, 此处只收目标根路径 + 业务参数; 类型化返回,
/// customTargetDir URL 后缀等展示逻辑归各域拼装层)。
pub async fn get_file_list_core(
    state: &AppState,
    path: &Path,
    p: FileListParams<'_>,
) -> Result<FileListOutcome, AppError> {
    // 参数校验先于目录不存在的空列表早返回；无效过滤条件必须明确报错。
    let file_type = parse_file_list_type(p.file_type)?;
    let limit = parse_file_list_limit(p.limit)?;
    // 默认 true=原全量递归; 仅显式 "false" 时单层 (对齐 TS recursive === false || recursive === "false")。
    // 注: query 参数经 serde 解析均为字符串, 故只需匹配 "false"。
    // 提前计算: 所有返回点 (含目录不存在的早返回) 都需带上 recursive (对齐 TS 1.3.7)。
    let recursive = !matches!(p.recursive, Some("false"));
    // 对齐 nuwax: 目标根目录不存在 → 返回空数组 (非报错), 带 recursive
    if !crate::service::fs_util::path_exists(path).await? {
        return Ok(FileListOutcome {
            files: Vec::new(),
            recursive,
            file_type,
            limit,
        });
    }
    // list_files_meta 内部解析 relativePath (越界 / 非目录抛 ValidationError → 400)。
    let files = tree::list_files_meta_filtered(
        path,
        &state.config,
        p.proxy_path,
        p.relative_path,
        tree::MetaListOptions {
            recursive,
            file_type,
            limit,
        },
    )
    .await?;
    Ok(FileListOutcome {
        files,
        recursive,
        file_type,
        limit,
    })
}

/// get-file-list 的 workspace 无关实现 (computer 域 TS 响应拼装)。
pub async fn get_file_list_impl(
    state: &AppState,
    path: &Path,
    p: FileListParams<'_>,
) -> Result<Json<FileListResult>, AppError> {
    let mut result = get_file_list_core(state, path, p).await?;
    let ct = trimmed_non_empty(p.custom_target_dir);
    // fileProxyUrl 追加 ?customTargetDir (对齐 nuwax; 值需 encodeURIComponent)。
    // 单层/递归模式统一在此补齐后缀。
    if let Some(ct) = ct {
        let suffix = format!(
            "?customTargetDir={}",
            code_service::encode_uri_component(ct)
        );
        for f in result.files.iter_mut() {
            if let Some(u) = f.file_proxy_url.as_mut() {
                u.push_str(&suffix);
            }
        }
    }
    Ok(Json(FileListResult {
        success: true,
        files: computer_file_entries(result.files),
        recursive: result.recursive,
        file_type: result.file_type.as_str().to_string(),
        limit: result.limit,
    }))
}

/// resolve-file 命中结果（file_proxy_url 为预览 URL，未含 customTargetDir 后缀）。
pub struct ResolvedFile {
    pub name: String,
    pub file_proxy_url: Option<String>,
}

/// resolve-file 的 workspace 无关核心。
pub async fn resolve_file_core(
    path: PathBuf,
    file_path: &str,
    proxy_path: Option<&str>,
) -> Result<Option<ResolvedFile>, AppError> {
    // 目标根目录不存在 → exists:false (对齐 TS)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(None);
    }
    let r = tree::resolve_existing_file(&path, file_path, proxy_path).await?;
    Ok(r.map(|r| ResolvedFile {
        name: r.name,
        file_proxy_url: r.file_proxy_url,
    }))
}

/// resolve-file 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn resolve_file_impl(
    path: PathBuf,
    file_path: &str,
    proxy_path: Option<&str>,
    custom_target_dir: Option<&str>,
) -> Result<Json<ResolveFileResult>, AppError> {
    let mut r = match resolve_file_core(path, file_path, proxy_path).await? {
        Some(r) => r,
        None => {
            return Ok(Json(ResolveFileResult::Missing {
                success: true,
                exists: false,
            }));
        }
    };
    // customTargetDir 后缀统一在此追加 (对齐 nuwax)
    if let (Some(ct), Some(url)) = (
        trimmed_non_empty(custom_target_dir),
        r.file_proxy_url.as_mut(),
    ) {
        url.push_str("?customTargetDir=");
        url.push_str(&code_service::encode_uri_component(ct));
    }
    Ok(Json(ResolveFileResult::Found {
        success: true,
        exists: true,
        name: r.name,
        file_proxy_url: r.file_proxy_url,
    }))
}

/// search-files 的 workspace 无关实现。
#[derive(Clone, Copy)]
pub struct SearchFilesParams<'a> {
    pub proxy_path: Option<&'a str>,
    pub relative_path: Option<&'a str>,
    pub kw: &'a str,
    pub limit: &'a str,
    pub max_visit: &'a str,
    pub timeout_ms: &'a str,
    pub custom_target_dir: Option<&'a str>,
}

/// search-files 结果（customTargetDir 后缀归各域拼装层）。
pub struct SearchOutcome {
    pub files: Vec<tree::FileEntry>,
    pub truncated: bool,
    pub visited: usize,
}

/// search-files 的 workspace 无关核心（`custom_target_dir` 不消费——URL 后缀
/// 归各域拼装层）。
pub async fn search_files_core(
    state: &AppState,
    path: PathBuf,
    p: SearchFilesParams<'_>,
) -> Result<SearchOutcome, AppError> {
    // garde positive_int 已保证正整数; 此处仅取数 (parse 失败逻辑不可达, 防御性处理)
    let limit = parse_positive_int(p.limit, "limit")?;
    let max_visit = parse_positive_int(p.max_visit, "maxVisit")?;
    let timeout_ms = parse_positive_int(p.timeout_ms, "timeoutMs")?;
    // 目标根目录不存在 → 空 (对齐 TS)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(SearchOutcome {
            files: Vec::new(),
            truncated: false,
            visited: 0,
        });
    }
    let result = tree::search_files(tree::SearchParams {
        root: &path,
        config: &state.config,
        proxy_path: p.proxy_path,
        kw: p.kw,
        relative_path: p.relative_path,
        limit,
        max_visit,
        timeout_ms: timeout_ms as u64,
    })
    .await?;
    Ok(SearchOutcome {
        files: result.files,
        truncated: result.truncated,
        visited: result.visited,
    })
}

/// search-files 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn search_files_impl(
    state: &AppState,
    path: PathBuf,
    p: SearchFilesParams<'_>,
) -> Result<Json<SearchFilesResult>, AppError> {
    let mut r = search_files_core(state, path, p).await?;
    let ct = trimmed_non_empty(p.custom_target_dir);
    // customTargetDir 后缀统一在此追加 (对齐 nuwax)
    if let Some(ct) = ct {
        let suffix = format!(
            "?customTargetDir={}",
            code_service::encode_uri_component(ct)
        );
        for f in r.files.iter_mut() {
            if let Some(u) = f.file_proxy_url.as_mut() {
                u.push_str(&suffix);
            }
        }
    }
    Ok(Json(SearchFilesResult {
        success: true,
        files: computer_file_entries(r.files),
        truncated: r.truncated,
        visited: r.visited,
    }))
}

/// Shape Computer file-list/search entries like the TypeScript API:
/// files always carry `fileProxyUrl` (null when proxyPath is absent) and `isLink`;
/// directory entries intentionally contain only `name` and `isDir`.
fn computer_file_entries(files: Vec<tree::FileEntry>) -> Vec<ComputerFileEntry> {
    files.into_iter().map(ComputerFileEntry::from).collect()
}

/// trim 后非空才返回 (customTargetDir 的 URL 后缀语义)。
pub fn trimmed_non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// 取数 helper: 与 garde `positive_int` 规则配套 (校验已通过, parse 失败逻辑不可达)。
fn parse_positive_int(value: &str, field: &str) -> Result<usize, AppError> {
    value
        .trim()
        .parse::<usize>()
        .map_err(|_| AppError::system(format!("{field}: parse failed after garde validation")))
}

// ── get-file-meta (对齐 TS 1.5.0 getFileMeta) ──────────────────────────────────

/// get-file-meta 单次批量缺省上限 (调用方未显式下发 fileMetaMaxBatch 时生效)。
pub const FILE_META_DEFAULT_MAX_BATCH: usize = 100;
/// get-file-meta 单次批量硬顶: 调用方下发的上限也压在该值内 (防畸形配置放大
/// lstat/响应开销)。
pub const FILE_META_HARD_MAX_BATCH: usize = 1000;
/// 单条内 lstat/readlink/read_dir 的条目并发上限 (IO 饱和保护, 对齐 TS
/// FILE_META_CONCURRENCY 的 libuv 线程池保护意图)。
const FILE_META_CONCURRENCY: usize = 8;
/// 目录子项计数截断上限: 防超大目录拖垮单请求; 达到即停, 值语义为 ≥LIMIT。
pub const FILE_META_CHILD_COUNT_LIMIT: u64 = 1000;

/// 生效批量上限: 缺省 100; 显式 0/缺省回落缺省值, 正数压入硬顶 1000 内
/// (对齐 TS parseInt 语义中 0/非正数回落的分支; 非数值输入已在 serde 层
/// fail-fast, 不做 TS 的静默回落)。
pub fn effective_file_meta_max_batch(requested: Option<u64>) -> usize {
    requested
        .filter(|v| *v > 0)
        .map(|v| (v as usize).min(FILE_META_HARD_MAX_BATCH))
        .unwrap_or(FILE_META_DEFAULT_MAX_BATCH)
}

/// get-file-meta 单条结果 (typed; 失败条目 error 非空、其余字段全 None——
/// 形状与成功条目一致, 前端可无分支消费)。path 回显请求输入 (trimmed),
/// 保证响应与请求条目可按键/按序关联; 规范路径以 get-file-list 的 name 为准。
pub struct FileMetaEntry {
    pub path: String,
    pub is_dir: Option<bool>,
    pub is_link: Option<bool>,
    pub size: Option<u64>,
    pub mtime_ms: Option<f64>,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    pub link_target: Option<String>,
    pub child_count: Option<u64>,
    pub error: Option<String>,
}

impl FileMetaEntry {
    fn errored(path: String, error: String) -> Self {
        Self {
            path,
            is_dir: None,
            is_link: None,
            size: None,
            mtime_ms: None,
            extension: None,
            mime_type: None,
            link_target: None,
            child_count: None,
            error: Some(error),
        }
    }
}

/// get-file-meta 的 workspace 无关核心 (computer / userapp 域共用; 定位由各域
/// 壳层完成)。readdir 的目录项不含 size (POSIX 本就无), 列表不带 size、由本
/// 接口 lstat 补查, 只为用户实际查看的文件付费。根目录缺失 → 空 metas (与
/// get-file-list 空列表语义对齐); 条目并发 FILE_META_CONCURRENCY 且结果与
/// 请求同序 (`buffered` 保序)。
pub async fn get_file_meta_core(
    state: &AppState,
    target_dir: &Path,
    file_paths: &[String],
) -> Result<Vec<FileMetaEntry>, AppError> {
    if !crate::service::fs_util::path_exists(target_dir).await? {
        return Ok(Vec::new());
    }
    let dir = target_dir.to_path_buf();
    let config = state.config.clone();
    let metas: Vec<FileMetaEntry> = futures_util::stream::iter(file_paths.iter().cloned())
        .map(|file_path| {
            let dir = dir.clone();
            let config = config.clone();
            async move { query_file_meta_entry(&dir, &file_path, &config).await }
        })
        .buffered(FILE_META_CONCURRENCY)
        .collect()
        .await;
    Ok(metas)
}

/// get-file-meta 的 workspace 无关实现 (computer 域 TS 响应拼装, camelCase 键;
/// error 仅失败条目携带, 成功条目无该键——对齐 TS 展开写法)。
pub async fn get_file_meta_impl(
    state: &AppState,
    path: &Path,
    file_paths: &[String],
) -> Result<Json<FileMetaResult>, AppError> {
    let metas = get_file_meta_core(state, path, file_paths).await?;
    Ok(Json(FileMetaResult {
        success: true,
        metas: metas.into_iter().map(FileMetaEntryResult::from).collect(),
    }))
}

impl From<FileMetaEntry> for FileMetaEntryResult {
    fn from(m: FileMetaEntry) -> Self {
        Self {
            path: m.path,
            is_dir: m.is_dir,
            is_link: m.is_link,
            size: m.size,
            mtime_ms: m.mtime_ms,
            extension: m.extension,
            mime_type: m.mime_type,
            link_target: m.link_target,
            child_count: m.child_count,
            error: m.error,
        }
    }
}

/// 单条元数据查询: 非法路径/不存在仅该条带 error。lstat 不跟随符号链接
/// (isLink 如实返回, size 为链接条目自身); 目录 size 恒 null (递归总大小需
/// 整棵子树遍历, 前端可基于扁平列表自行聚合)。
async fn query_file_meta_entry(
    target_dir: &Path,
    file_path: &str,
    config: &Config,
) -> FileMetaEntry {
    // 有意偏离 TS：不做整体 trim——会静默变形以空白开头/结尾的合法路径段。
    // 判空保持 TS 口径（纯空白 → illegal），非纯空白原样解析
    let input = file_path;
    if input.trim().is_empty() {
        return FileMetaEntry::errored(String::new(), "illegal path".into());
    }
    // resolve_subdir 内含前导斜杠剥除 + `..` 拒绝 + ensure_within 兜底, 与 TS
    // resolveFilePathWithinWorkspace + `/` 前缀重试同语义; 越界 → illegal path。
    let resolved = match tree::resolve_subdir(target_dir, Some(input)) {
        Ok(resolved) => resolved,
        Err(_) => return FileMetaEntry::errored(input.to_string(), "illegal path".into()),
    };
    let meta = match tokio::fs::symlink_metadata(&resolved).await {
        Ok(meta) => meta,
        Err(e) => return FileMetaEntry::errored(input.to_string(), e.to_string()),
    };
    let is_dir = meta.is_dir();
    let is_link = meta.is_symlink();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0);
    let mut entry = FileMetaEntry {
        path: input.to_string(),
        is_dir: Some(is_dir),
        is_link: Some(is_link),
        size: if is_dir { None } else { Some(meta.len()) },
        mtime_ms,
        extension: if is_dir {
            None
        } else {
            Some(extension_of(input))
        },
        mime_type: if is_dir {
            None
        } else {
            Some(mime_type_of(input))
        },
        link_target: None,
        child_count: None,
        error: None,
    };
    if is_link {
        match tokio::fs::read_link(&resolved).await {
            Ok(target) => entry.link_target = Some(target.to_string_lossy().into_owned()),
            Err(e) => tracing::warn!(
                path = %input,
                error = %e,
                "readlink failed, linkTarget set to null"
            ),
        }
    }
    if is_dir {
        match count_visible_dir_entries(&resolved, config).await {
            Ok(count) => entry.child_count = Some(count),
            Err(e) => tracing::warn!(
                path = %input,
                error = %e,
                "count directory entries failed, childCount set to null"
            ),
        }
    }
    entry
}

/// 提取小写扩展名 (不含点; 无扩展名返回空串——对齐 TS extensionOf)。
fn extension_of(input: &str) -> String {
    Path::new(input)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 按文件名查表 MIME (零 syscall); 未知类型回落 application/octet-stream。
fn mime_type_of(input: &str) -> String {
    mime_guess::from_path(input)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// 目录子项流式计数 (不物化整个目录), 达到截断上限提前返回。可见性与文件
/// 列表同口径 (隐藏项保留 .gitignore、排除名单整体生效), 但**只按名字过滤
/// 不 stat 类型**——与排除目录同名的文件被误排属可接受的极端情况 (对齐 TS
/// countDirEntries; 刻意不复用 `read_filtered_entries`, 其 stat 类型+物化+排序)。
async fn count_visible_dir_entries(dir: &Path, config: &Config) -> Result<u64, AppError> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut count: u64 = 0;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if !is_counted_dir_entry(&name.to_string_lossy(), config) {
            continue;
        }
        count += 1;
        if count >= FILE_META_CHILD_COUNT_LIMIT {
            return Ok(FILE_META_CHILD_COUNT_LIMIT);
        }
    }
    Ok(count)
}

fn is_counted_dir_entry(name: &str, config: &Config) -> bool {
    if name.starts_with('.') && name != tree::KEEP_HIDDEN_FILE {
        return false;
    }
    if config
        .content_traverse_exclude_files
        .iter()
        .any(|f| f == name)
    {
        return false;
    }
    if config.traverse_exclude_dirs.iter().any(|d| d == name) {
        return false;
    }
    true
}

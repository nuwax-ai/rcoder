//! 文件读类共享实现：get-file-list / resolve-file / search-files。
//!
//! 壳与 handler 层测试在 handlers/computer/files_read.rs；参数经
//! `FileListParams` / `SearchFilesParams` 借用结构传入，定位（computer 树或
//! userapp 开发卷）由各域壳层完成。

use std::path::{Path, PathBuf};

use crate::extract::AppJson as Json;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::service::{code as code_service, tree};

pub struct FileListParams<'a> {
    pub proxy_path: Option<&'a str>,
    pub relative_path: Option<&'a str>,
    /// 原始 recursive 串: 缺省/非 "false" 均按递归 (对齐 TS)。
    pub recursive: Option<&'a str>,
    /// 原始 customTargetDir 串: 仅用于 fileProxyUrl 后缀 (定位语义由壳层消化)。
    pub custom_target_dir: Option<&'a str>,
}

/// get-file-list 的 workspace 无关核心 (computer / userapp 域共用;
/// 定位由各域壳层完成, 此处只收目标根路径 + 业务参数; 类型化返回,
/// customTargetDir URL 后缀等展示逻辑归各域拼装层)。
pub async fn get_file_list_core(
    state: &AppState,
    path: &Path,
    proxy_path: Option<&str>,
    relative_path: Option<&str>,
    recursive: Option<&str>,
) -> Result<(Vec<tree::FileEntry>, bool), AppError> {
    // 默认 true=原全量递归; 仅显式 "false" 时单层 (对齐 TS recursive === false || recursive === "false")。
    // 注: query 参数经 serde 解析均为字符串, 故只需匹配 "false"。
    // 提前计算: 所有返回点 (含目录不存在的早返回) 都需带上 recursive (对齐 TS 1.3.7)。
    let is_recursive = !matches!(recursive, Some("false"));
    // 对齐 nuwax: 目标根目录不存在 → 返回空数组 (非报错), 带 recursive
    if !crate::service::fs_util::path_exists(path).await? {
        return Ok((Vec::new(), is_recursive));
    }
    // list_files_meta 内部解析 relativePath (越界 / 非目录抛 ValidationError → 400)。
    let files = tree::list_files_meta(path, &state.config, proxy_path, relative_path, is_recursive)
        .await?;
    Ok((files, is_recursive))
}

/// get-file-list 的 workspace 无关实现 (computer 域 TS 响应拼装)。
pub async fn get_file_list_impl(
    state: &AppState,
    path: &Path,
    p: FileListParams<'_>,
) -> Result<Json<Value>, AppError> {
    let (mut files, is_recursive) = get_file_list_core(
        state,
        path,
        p.proxy_path,
        p.relative_path,
        p.recursive,
    )
    .await?;
    let ct = trimmed_non_empty(p.custom_target_dir);
    // fileProxyUrl 追加 ?customTargetDir (对齐 nuwax; 值需 encodeURIComponent)。
    // 单层/递归模式统一在此补齐后缀。
    if let Some(ct) = ct {
        let suffix = format!(
            "?customTargetDir={}",
            code_service::encode_uri_component(ct)
        );
        for f in files.iter_mut() {
            if let Some(u) = f.file_proxy_url.as_mut() {
                u.push_str(&suffix);
            }
        }
    }
    Ok(Json(
        json!({ "success": true, "files": files, "recursive": is_recursive }),
    ))
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
) -> Result<Json<Value>, AppError> {
    let mut r = match resolve_file_core(path, file_path, proxy_path).await? {
        Some(r) => r,
        None => return Ok(Json(json!({ "success": true, "exists": false }))),
    };
    // customTargetDir 后缀统一在此追加 (对齐 nuwax)
    if let (Some(ct), Some(url)) = (
        trimmed_non_empty(custom_target_dir),
        r.file_proxy_url.as_mut(),
    ) {
        url.push_str("?customTargetDir=");
        url.push_str(&code_service::encode_uri_component(ct));
    }
    Ok(Json(json!({
        "success": true,
        "exists": true,
        "name": r.name,
        "fileProxyUrl": r.file_proxy_url,
    })))
}

/// search-files 的 workspace 无关实现。
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

/// search-files 的 workspace 无关核心。
pub async fn search_files_core(
    state: &AppState,
    path: PathBuf,
    proxy_path: Option<&str>,
    relative_path: Option<&str>,
    kw: &str,
    limit: &str,
    max_visit: &str,
    timeout_ms: &str,
) -> Result<SearchOutcome, AppError> {
    // garde positive_int 已保证正整数; 此处仅取数 (parse 失败逻辑不可达, 防御性处理)
    let limit = parse_positive_int(limit, "limit")?;
    let max_visit = parse_positive_int(max_visit, "maxVisit")?;
    let timeout_ms = parse_positive_int(timeout_ms, "timeoutMs")?;
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
        proxy_path,
        kw,
        relative_path,
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
) -> Result<Json<Value>, AppError> {
    let mut r = search_files_core(
        state,
        path,
        p.proxy_path,
        p.relative_path,
        p.kw,
        p.limit,
        p.max_visit,
        p.timeout_ms,
    )
    .await?;
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
    Ok(Json(json!({
        "success": true,
        "files": r.files,
        "truncated": r.truncated,
        "visited": r.visited,
    })))
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

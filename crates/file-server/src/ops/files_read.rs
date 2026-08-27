//! 文件读类共享实现：get-file-list / resolve-file / search-files。
//!
//! 自 handlers/computer/files_read.rs 抽出（壳与测试留守原处）；参数经
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

/// get-file-list 的 workspace 无关实现 (computer / userapp 域共用;
/// 定位由各域壳层完成, 此处只收目标根路径 + 业务参数)。
pub async fn get_file_list_impl(
    state: &AppState,
    path: &Path,
    p: FileListParams<'_>,
) -> Result<Json<Value>, AppError> {
    // 默认 true=原全量递归; 仅显式 "false" 时单层 (对齐 TS recursive === false || recursive === "false")。
    // 注: query 参数经 serde 解析均为字符串, 故只需匹配 "false"。
    // 提前计算: 所有返回点 (含目录不存在的早返回) 都需带上 recursive (对齐 TS 1.3.7)。
    let is_recursive = !matches!(p.recursive, Some("false"));
    // 对齐 nuwax: 目标根目录不存在 → 返回空数组 (非报错), 带 recursive
    if !crate::service::fs_util::path_exists(path).await? {
        return Ok(Json(
            json!({ "success": true, "files": [], "recursive": is_recursive }),
        ));
    }
    let ct = trimmed_non_empty(p.custom_target_dir);
    // list_files_meta 内部解析 relativePath (越界 / 非目录抛 ValidationError → 400)。
    let mut files = tree::list_files_meta(
        path,
        &state.config,
        p.proxy_path,
        p.relative_path,
        is_recursive,
    )
    .await?;
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

/// resolve-file 的 workspace 无关实现。
pub async fn resolve_file_impl(
    path: PathBuf,
    file_path: &str,
    proxy_path: Option<&str>,
    custom_target_dir: Option<&str>,
) -> Result<Json<Value>, AppError> {
    // 目标根目录不存在 → exists:false (对齐 TS)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(Json(json!({ "success": true, "exists": false })));
    }
    let ct = trimmed_non_empty(custom_target_dir);
    let result = tree::resolve_existing_file(&path, file_path, proxy_path).await?;
    match result {
        Some(mut r) => {
            // customTargetDir 后缀统一在此追加 (对齐 nuwax)
            if let (Some(ct), Some(url)) = (ct, r.file_proxy_url.as_mut()) {
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
        None => Ok(Json(json!({ "success": true, "exists": false }))),
    }
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

pub async fn search_files_impl(
    state: &AppState,
    path: PathBuf,
    p: SearchFilesParams<'_>,
) -> Result<Json<Value>, AppError> {
    // garde positive_int 已保证正整数; 此处仅取数 (parse 失败逻辑不可达, 防御性处理)
    let limit = parse_positive_int(p.limit, "limit")?;
    let max_visit = parse_positive_int(p.max_visit, "maxVisit")?;
    let timeout_ms = parse_positive_int(p.timeout_ms, "timeoutMs")?;

    let ct = trimmed_non_empty(p.custom_target_dir);
    // 目标根目录不存在 → 空 (对齐 TS)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(Json(json!({
            "success": true,
            "files": [],
            "truncated": false,
            "visited": 0
        })));
    }
    let mut result = tree::search_files(tree::SearchParams {
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
    // customTargetDir 后缀统一在此追加 (对齐 nuwax)
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
    Ok(Json(json!({
        "success": true,
        "files": result.files,
        "truncated": result.truncated,
        "visited": result.visited,
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

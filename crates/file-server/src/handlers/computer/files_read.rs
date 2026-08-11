//! computer 文件**读取类** handlers: get-file-list / resolve-file / search-files。
//!
//! 从 `files` 拆出 (读/写分离, 避免单文件膨胀)。写类 handler (delete-workspace /
//! files-update / upload / generate-file / import-project) 仍留在 [`super::files`]。

use axum::extract::State;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::{code as code_service, tree};

use super::{FileListQuery, ResolveFileQuery, SearchFilesQuery, resolve_computer_target};

// ── get-file-list ───────────────────────────────────────────────────────────────

/// `GET /api/computer/get-file-list` (对齐 nuwax getFileList, 含 commit ba08d0c 增强):
/// 轻量元信息遍历 (不读内容) + customTargetDir 覆盖 + relativePath 子目录 + recursive 单层开关;
/// 目录不存在返回空数组。
#[utoipa::path(
    get,
    path = "/get-file-list",
    params(FileListQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn get_file_list(
    State(state): State<AppState>,
    Query(q): Query<FileListQuery>,
) -> Result<Json<Value>, AppError> {
    // 默认 true=原全量递归; 仅显式 "false" 时单层 (对齐 TS recursive === false || recursive === "false")。
    // 注: query 参数经 serde 解析均为字符串, 故只需匹配 "false"。
    // 提前计算: 所有返回点 (含目录不存在的早返回) 都需带上 recursive (对齐 TS 1.3.7)。
    let is_recursive = !matches!(q.recursive.as_deref(), Some("false"));
    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    // 对齐 nuwax: 目标根目录不存在 → 返回空数组 (非报错), 带 recursive
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(Json(
            json!({ "success": true, "files": [], "recursive": is_recursive }),
        ));
    }
    let ct = q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // list_files_meta 内部解析 relativePath (越界 / 非目录抛 ValidationError → 400)。
    let mut files = tree::list_files_meta(
        &path,
        &state.config,
        q.proxy_path.as_deref(),
        q.relative_path.as_deref(),
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

// ── resolve-file ────────────────────────────────────────────────────────────────

/// `GET /api/computer/resolve-file` (对齐 nuwax resolveExistingFile, commit ba08d0c):
/// 校验目标根目录下文件是否存在, 存在返回 `{exists:true, name, fileProxyUrl}`, 否则 `{exists:false}`。
#[utoipa::path(
    get,
    path = "/resolve-file",
    params(ResolveFileQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn resolve_file(
    State(state): State<AppState>,
    Query(q): Query<ResolveFileQuery>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    let file_path = q.file_path.trim();
    if file_path.is_empty() {
        return Err(AppError::validation("filePath cannot be empty"));
    }
    // 目标根目录不存在 → exists:false (对齐 TS)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(Json(json!({ "success": true, "exists": false })));
    }
    let ct = q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let result = tree::resolve_existing_file(&path, file_path, q.proxy_path.as_deref()).await?;
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

// ── search-files ────────────────────────────────────────────────────────────────

/// `GET /api/computer/search-files` (对齐 nuwax searchFiles, commit ba08d0c):
/// 无索引有界实时搜索; `limit`/`maxVisit`/`timeoutMs` 为必填正整数 (由网关传入)。
#[utoipa::path(
    get,
    path = "/search-files",
    params(SearchFilesQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn search_files(
    State(state): State<AppState>,
    Query(q): Query<SearchFilesQuery>,
) -> Result<Json<Value>, AppError> {
    let kw = q.kw.trim();
    if kw.is_empty() {
        return Err(AppError::validation("kw cannot be empty"));
    }
    // 对齐 TS requirePositiveInt: 必填正整数
    let limit = require_positive_int(&q.limit, "limit")?;
    let max_visit = require_positive_int(&q.max_visit, "maxVisit")?;
    let timeout_ms = require_positive_int(&q.timeout_ms, "timeoutMs")?;

    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    let ct = q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
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
        proxy_path: q.proxy_path.as_deref(),
        kw,
        relative_path: q.relative_path.as_deref(),
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

/// 必填正整数校验 (对齐 TS `requirePositiveInt`)。
fn require_positive_int(value: &str, field: &str) -> Result<usize, AppError> {
    let n = value
        .trim()
        .parse::<usize>()
        .map_err(|_| AppError::validation(format!("{field} must be a positive integer")))?;
    if n == 0 {
        return Err(AppError::validation(format!(
            "{field} must be a positive integer"
        )));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{
        AppState, BuildManager, BuildTaskStore, Config, DevServerManager, LocalWorkspaceResolver,
        LogCacheManager, SkillDownloader, WorkspaceResolver,
    };

    /// 构造一个指向临时目录的 AppState (computer root = temp)，镜像 FileServerBuilder::build。
    fn make_state(computer_root: std::path::PathBuf) -> AppState {
        let config = Arc::new(Config::default());
        let resolver: Arc<dyn WorkspaceResolver> = Arc::new(LocalWorkspaceResolver::new(
            config.project_source_dir.clone(),
            computer_root,
        ));
        AppState {
            resolver,
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader: Arc::new(
                SkillDownloader::new(&config).expect("construct skill downloader"),
            ),
            build_tasks: Arc::new(BuildTaskStore::new()),
            config,
            started_at: std::time::Instant::now(),
        }
    }

    /// 准备一个工作区并写入若干文件 (computer_root/u/c/...).
    async fn seed_workspace(computer_root: &std::path::Path) {
        let ws = computer_root.join("u").join("c");
        tokio::fs::create_dir_all(ws.join("sub")).await.unwrap();
        tokio::fs::write(ws.join("a.txt"), "a").await.unwrap();
        tokio::fs::write(ws.join("sub").join("c.txt"), "c")
            .await
            .unwrap();
    }

    // ── get_file_list handler 层测试 (参数解析 + recursive + customTargetDir 后缀) ──

    #[tokio::test]
    async fn get_file_list_default_recursive_flattens_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            recursive: None, // 缺省 = 递归
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        assert_eq!(val["recursive"], json!(true)); // 缺省 recursive=true
        let names: Vec<&str> = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        // 递归: sub/c.txt 应扁平展开
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub/c.txt"));
    }

    #[tokio::test]
    async fn get_file_list_recursive_false_single_level() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            recursive: Some("false".into()),
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        assert_eq!(val["recursive"], json!(false)); // 显式 false
        let names: Vec<&str> = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub")); // 子目录作为节点
        // 单层: 不展开 sub 的子文件
        assert!(!names.contains(&"sub/c.txt"));
    }

    #[tokio::test]
    async fn get_file_list_nonexistent_dir_returns_empty_with_recursive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        // 不 seed → 工作区不存在
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            recursive: None,
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        assert_eq!(val["files"], json!([]));
        // 早返回也带 recursive (对齐 TS 1.3.7)
        assert_eq!(val["recursive"], json!(true));
    }

    #[tokio::test]
    async fn get_file_list_custom_target_dir_suffix_appended() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let custom = tmp.path().join("custom-ws");
        tokio::fs::create_dir_all(&custom).await.unwrap();
        tokio::fs::write(custom.join("f.txt"), "x").await.unwrap();
        let state = make_state(tmp.path().join("c"));
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: Some("/proxy".into()),
            custom_target_dir: Some(custom.to_string_lossy().into_owned()),
            relative_path: None,
            recursive: Some("false".into()),
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        let file_entry = &val["files"][0];
        assert_eq!(file_entry["name"], "f.txt");
        // fileProxyUrl 应含 ?customTargetDir= 后缀
        assert!(
            file_entry["fileProxyUrl"]
                .as_str()
                .unwrap()
                .starts_with("/proxy/f.txt?customTargetDir=")
        );
    }

    #[tokio::test]
    async fn get_file_list_proxy_url_encoded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        let ws = computer_root.join("u").join("c");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("a b.txt"), "x").await.unwrap();
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: Some("/proxy".into()),
            custom_target_dir: None,
            relative_path: None,
            recursive: Some("false".into()),
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        let entry = &val["files"][0];
        // 空格 encode → %20
        assert_eq!(entry["name"], "a b.txt");
        assert_eq!(entry["fileProxyUrl"], "/proxy/a%20b.txt");
    }

    #[tokio::test]
    async fn resolve_file_returns_exists_true_for_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(ResolveFileQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: Some("/proxy".into()),
            custom_target_dir: None,
            file_path: "sub/c.txt".into(),
        });
        let res = resolve_file(State(state), q).await.expect("resolve ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        assert_eq!(val["exists"], json!(true));
        assert_eq!(val["name"], "sub/c.txt");
        assert_eq!(val["fileProxyUrl"], "/proxy/sub/c.txt");
    }

    #[tokio::test]
    async fn resolve_file_returns_exists_false_for_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(ResolveFileQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            file_path: "nope.txt".into(),
        });
        let res = resolve_file(State(state), q).await.expect("resolve ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        assert_eq!(val["exists"], json!(false));
        assert!(val.get("name").is_none());
    }

    #[tokio::test]
    async fn resolve_file_empty_file_path_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(ResolveFileQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            file_path: "   ".into(),
        });
        let err = resolve_file(State(state), q)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("filePath cannot be empty"));
    }

    #[tokio::test]
    async fn resolve_file_custom_target_dir_suffix_appended() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let custom = tmp.path().join("custom-ws");
        tokio::fs::create_dir_all(&custom).await.unwrap();
        tokio::fs::write(custom.join("f.txt"), "x").await.unwrap();
        let state = make_state(tmp.path().join("c"));
        let q = Query(ResolveFileQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: Some("/proxy".into()),
            custom_target_dir: Some(custom.to_string_lossy().into_owned()),
            file_path: "f.txt".into(),
        });
        let res = resolve_file(State(state), q).await.expect("resolve ok");
        let val = res.0;
        assert_eq!(val["exists"], json!(true));
        // customTargetDir 后缀需 encodeURIComponent
        assert!(
            val["fileProxyUrl"]
                .as_str()
                .unwrap()
                .starts_with("/proxy/f.txt?customTargetDir=")
        );
    }

    #[tokio::test]
    async fn search_files_returns_matching_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        let q = Query(SearchFilesQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: Some("/proxy".into()),
            custom_target_dir: None,
            relative_path: None,
            kw: ".txt".into(),
            limit: "100".into(),
            max_visit: "1000".into(),
            timeout_ms: "5000".into(),
        });
        let res = search_files(State(state), q).await.expect("search ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        let names: Vec<&str> = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub/c.txt"));
        assert_eq!(val["truncated"], json!(false));
    }

    #[tokio::test]
    async fn search_files_rejects_non_positive_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root);
        let q = Query(SearchFilesQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            kw: "x".into(),
            limit: "0".into(), // 非正
            max_visit: "1000".into(),
            timeout_ms: "5000".into(),
        });
        let err = search_files(State(state), q)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("limit must be a positive integer"));
    }

    #[tokio::test]
    async fn search_files_rejects_empty_kw() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root);
        let q = Query(SearchFilesQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            kw: "   ".into(),
            limit: "100".into(),
            max_visit: "1000".into(),
            timeout_ms: "5000".into(),
        });
        let err = search_files(State(state), q)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("kw cannot be empty"));
    }
}

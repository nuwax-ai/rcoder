//! computer 文件**读取类** handlers: get-file-list / resolve-file / search-files。
//!
//! 从 `files` 拆出 (读/写分离, 避免单文件膨胀)。写类 handler (delete-workspace /
//! files-update / upload / generate-file / import-project) 仍留在 [`super::files`]。

use axum::extract::State;
use garde::Validate;
use serde_json::Value;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{FileListQuery, GetFileMetaBody, ResolveFileQuery, SearchFilesQuery};

use crate::ops::files_read::{
    FileListParams, SearchFilesParams, effective_file_meta_max_batch, get_file_list_impl,
    get_file_meta_impl, resolve_file_impl, search_files_impl,
};

use super::ServiceScope;
use super::resolve_computer_target;

// ── get-file-list ───────────────────────────────────────────────────────────────

/// 获取文件列表
///
/// 对齐 nuwax getFileList：轻量元信息遍历、子目录、递归开关及
/// `type`/`limit` 扫描时过滤；目录不存在返回空数组并回显生效参数。
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
    q.validate().map_err(crate::error::from_garde)?;
    let path = resolve_computer_target(
        &state,
        &q.user_id,
        &q.c_id,
        q.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: q.workspace_type.as_deref(),
            service_type: q.service_type.as_deref(),
            app_id: q.app_id.as_deref(),
            workspace_path: q.workspace_path.as_deref(),
        },
    )
    .await?;
    get_file_list_impl(
        &state,
        &path,
        FileListParams {
            proxy_path: q.proxy_path.as_deref(),
            relative_path: q.relative_path.as_deref(),
            recursive: q.recursive.as_deref(),
            file_type: q.file_type.as_deref(),
            limit: q.limit.as_deref(),
            custom_target_dir: q.custom_target_dir.as_deref(),
        },
    )
    .await
}

// ── resolve-file ────────────────────────────────────────────────────────────────

/// 解析文件存在性
///
/// 对齐 nuwax resolveExistingFile, commit ba08d0c:
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
    q.validate().map_err(crate::error::from_garde)?;
    let path = resolve_computer_target(
        &state,
        &q.user_id,
        &q.c_id,
        q.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: q.workspace_type.as_deref(),
            service_type: q.service_type.as_deref(),
            app_id: q.app_id.as_deref(),
            workspace_path: q.workspace_path.as_deref(),
        },
    )
    .await?;
    resolve_file_impl(
        path,
        q.file_path.trim(),
        q.proxy_path.as_deref(),
        q.custom_target_dir.as_deref(),
    )
    .await
}

// ── search-files ────────────────────────────────────────────────────────────────

/// 搜索文件
///
/// 对齐 nuwax searchFiles, commit ba08d0c:
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
    q.validate().map_err(crate::error::from_garde)?;
    let path = resolve_computer_target(
        &state,
        &q.user_id,
        &q.c_id,
        q.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: q.workspace_type.as_deref(),
            service_type: q.service_type.as_deref(),
            app_id: q.app_id.as_deref(),
            workspace_path: q.workspace_path.as_deref(),
        },
    )
    .await?;
    search_files_impl(
        &state,
        path,
        SearchFilesParams {
            proxy_path: q.proxy_path.as_deref(),
            relative_path: q.relative_path.as_deref(),
            kw: q.kw.trim(),
            limit: &q.limit,
            max_visit: &q.max_visit,
            timeout_ms: &q.timeout_ms,
            custom_target_dir: q.custom_target_dir.as_deref(),
        },
    )
    .await
}

// ── get-file-meta ───────────────────────────────────────────────────────────────

/// 批量查询文件元数据
///
/// 对齐 nuwax 1.5.0 getFileMeta：大小/mtime/MIME/扩展名/软链目标/目录子项数；
/// 与 get-file-list 解耦按需查询；单条失败仅该条带 error，不影响整批。
#[utoipa::path(
    post,
    path = "/get-file-meta",
    request_body = GetFileMetaBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn get_file_meta(
    State(state): State<AppState>,
    Json(body): Json<GetFileMetaBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    // 非空与上限联合校验 (对齐 TS ValidationError 位置; 上限缺省 100/硬顶 1000)
    if body.file_paths.is_empty() {
        return Err(AppError::validation("filePaths must be a non-empty array"));
    }
    let max_batch = effective_file_meta_max_batch(body.file_meta_max_batch);
    if body.file_paths.len() > max_batch {
        return Err(AppError::validation(format!(
            "filePaths batch size {} exceeds limit {max_batch}",
            body.file_paths.len()
        )));
    }
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    get_file_meta_impl(&state, &path, &body.file_paths).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::json;

    use crate::{
        AppState, BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
        SkillDownloader, WorkspaceResolver,
    };

    /// 构造一个指向临时目录的 AppState (computer root = temp)，镜像 FileServerBuilder::build。
    fn make_state(computer_root: PathBuf) -> AppState {
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
            config,
            preview: None,
            started_at: std::time::Instant::now(),
        }
    }

    /// 准备一个工作区并写入若干文件 (computer_root/u/c/...).
    async fn seed_workspace(computer_root: &Path) {
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: None, // 缺省 = 递归
            file_type: None,
            limit: None,
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));
        assert_eq!(val["recursive"], json!(true)); // 缺省 recursive=true
        assert_eq!(val["type"], json!("all"));
        assert_eq!(val["limit"], json!(null));
        let names: Vec<&str> = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        // 递归: sub/c.txt 应扁平展开
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub/c.txt"));
        let file = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "a.txt")
            .unwrap();
        assert_eq!(file["fileProxyUrl"], json!(null));
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: Some("false".into()),
            file_type: None,
            limit: None,
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: None,
            file_type: None,
            limit: None,
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: Some("false".into()),
            file_type: None,
            limit: None,
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: Some("false".into()),
            file_type: Some("FiLe".into()),
            limit: Some("1".into()),
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let val = res.0;
        let entry = &val["files"][0];
        assert_eq!(val["type"], json!("file"));
        assert_eq!(val["limit"], json!(1));
        assert_eq!(val["files"].as_array().map(Vec::len), Some(1));
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            file_path: "".into(),
        });
        let err = resolve_file(State(state), q)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("file_path"));
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
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
            proxy_path: None,
            custom_target_dir: None,
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
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
        let file = val["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "a.txt")
            .unwrap();
        assert_eq!(file["fileProxyUrl"], json!(null));
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
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
        assert!(err.to_string().contains("must be a positive integer"));
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
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            kw: "".into(),
            limit: "100".into(),
            max_visit: "1000".into(),
            timeout_ms: "5000".into(),
        });
        let err = search_files(State(state), q)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("kw"));
    }

    // ── 项目绑定目录通道 (对齐 TS 1.4.5) ─────────────────────────────────────────

    /// query 显式 workspacePath → 工作区切到绑定目录 (不经 user/cid 默认定位)。
    #[tokio::test]
    async fn get_file_lists_bound_dir_when_workspace_path_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_workspace(&computer_root).await;
        // 绑定目录: 独立于 computer_root 的另一棵树
        let bound = tmp.path().join("bound-ws");
        tokio::fs::create_dir_all(&bound).await.unwrap();
        tokio::fs::write(bound.join("only-in-bound.txt"), "x")
            .await
            .unwrap();

        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            workspace_path: Some(bound.to_string_lossy().into_owned()),
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: None,
            file_type: None,
            limit: None,
        });
        let res = get_file_list(State(state), q).await.expect("list ok");
        let names: Vec<&str> = res.0["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        // 列的是绑定目录内容, 不是默认工作区
        assert_eq!(names, vec!["only-in-bound.txt"]);
    }

    /// 非法绑定 (相对路径) → fail-fast 400, 不落盘不遍历。
    #[tokio::test]
    async fn get_file_rejects_relative_workspace_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        let q = Query(FileListQuery {
            user_id: "u".into(),
            c_id: "c".into(),
            proxy_path: None,
            custom_target_dir: None,
            workspace_path: Some("relative/nope".into()),
            service_type: None,
            workspace_type: None,
            app_id: None,
            relative_path: None,
            recursive: None,
            file_type: None,
            limit: None,
        });
        let err = match get_file_list(State(state), q).await {
            Err(e) => e,
            Ok(_) => panic!("relative workspacePath must be rejected"),
        };
        use axum::response::IntoResponse;
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    // ── get_file_meta handler 层测试 (对齐 TS 1.5.0 getFileMeta) ─────────────────

    fn meta_body(file_paths: Vec<&str>) -> Json<GetFileMetaBody> {
        Json(GetFileMetaBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_paths: file_paths.into_iter().map(Into::into).collect(),
            file_meta_max_batch: None,
            custom_target_dir: None,
            workspace_path: None,
            service_type: None,
            workspace_type: None,
            app_id: None,
        })
    }

    /// 元数据种子: a.txt / IMG.PNG / sub/(c.txt + .gitignore + .hidden +
    /// node_modules/) + unix 软链 link→a.txt。
    async fn seed_meta_workspace(computer_root: &Path) {
        let ws = computer_root.join("u").join("c");
        tokio::fs::create_dir_all(ws.join("sub").join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(ws.join("a.txt"), "abc").await.unwrap();
        tokio::fs::write(ws.join("IMG.PNG"), "x").await.unwrap();
        tokio::fs::write(ws.join("sub").join("c.txt"), "c")
            .await
            .unwrap();
        tokio::fs::write(ws.join("sub").join(".gitignore"), "g")
            .await
            .unwrap();
        tokio::fs::write(ws.join("sub").join(".hidden"), "h")
            .await
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", ws.join("link")).unwrap();
    }

    fn meta_of<'a>(val: &'a Value, path: &str) -> &'a Value {
        val["metas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["path"].as_str() == Some(path))
            .unwrap_or_else(|| panic!("meta {path} missing"))
    }

    #[tokio::test]
    async fn get_file_meta_file_dir_symlink_shapes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_meta_workspace(&computer_root).await;
        let res = get_file_meta(
            State(state),
            meta_body(vec!["a.txt", "IMG.PNG", "sub", "link"]),
        )
        .await
        .expect("meta ok");
        let val = res.0;
        assert_eq!(val["success"], json!(true));

        // 常规文件: size/mtimeMs/extension 小写/mime 查表, 无 error 键
        let f = meta_of(&val, "a.txt");
        assert_eq!(f["isDir"], json!(false));
        assert_eq!(f["size"], json!(3));
        assert!(f["mtimeMs"].as_f64().unwrap() > 0.0);
        assert_eq!(f["extension"], json!("txt"));
        assert_eq!(f["mimeType"], json!("text/plain"));
        assert!(f.get("error").is_none());

        // 大写扩展名归一小写
        let img = meta_of(&val, "IMG.PNG");
        assert_eq!(img["extension"], json!("png"));
        assert_eq!(img["mimeType"], json!("image/png"));

        // 目录: size/extension/mimeType 恒 null; childCount 可见性口径
        // (c.txt + .gitignore 计入; .hidden 隐藏、node_modules 排除)
        let d = meta_of(&val, "sub");
        assert_eq!(d["isDir"], json!(true));
        assert_eq!(d["size"], json!(null));
        assert_eq!(d["extension"], json!(null));
        assert_eq!(d["mimeType"], json!(null));
        assert_eq!(d["childCount"], json!(2));

        // 软链: isLink 如实 + linkTarget 回显; size 为链接条目自身 (lstat)
        #[cfg(unix)]
        {
            let l = meta_of(&val, "link");
            assert_eq!(l["isLink"], json!(true));
            assert_eq!(l["linkTarget"], json!("a.txt"));
            assert!(l["size"].as_u64().is_some());
        }
    }

    #[tokio::test]
    async fn get_file_meta_missing_entry_error_isolated_and_order_preserved() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_meta_workspace(&computer_root).await;
        let res = get_file_meta(State(state), meta_body(vec!["a.txt", "nope.txt", "sub"]))
            .await
            .expect("meta ok");
        let val = res.0;
        // 响应与请求同序 (按键关联的兜底契约)
        let paths: Vec<&str> = val["metas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["a.txt", "nope.txt", "sub"]);
        // 单条失败仅该条带 error, 其余字段 null, 不影响整批
        let missing = meta_of(&val, "nope.txt");
        assert!(missing["error"].as_str().is_some_and(|e| !e.is_empty()));
        assert_eq!(missing["size"], json!(null));
        assert_eq!(meta_of(&val, "a.txt")["size"], json!(3));
    }

    #[tokio::test]
    async fn get_file_meta_traversal_and_blank_are_illegal_per_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        seed_meta_workspace(&computer_root).await;
        let res = get_file_meta(State(state), meta_body(vec!["../x", "  ", "/a.txt"]))
            .await
            .expect("meta ok");
        let val = res.0;
        // 穿越 → illegal path
        assert_eq!(meta_of(&val, "../x")["error"], json!("illegal path"));
        // 空白串 trim 后为空 → illegal path (path 回显 trimmed 空串)
        assert_eq!(meta_of(&val, "")["error"], json!("illegal path"));
        // / 开头实为相对目标根的写法兼容 (resolve_subdir 剥前导斜杠);
        // path 回显原始输入 (对齐 TS: 规范路径以 get-file-list 的 name 为准)
        let abs = meta_of(&val, "/a.txt");
        assert_eq!(abs["size"], json!(3));
        assert!(abs.get("error").is_none());
    }

    #[tokio::test]
    async fn get_file_meta_rejects_empty_and_over_limit_batch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        // 空数组 → 400
        let err = get_file_meta(State(state.clone()), meta_body(vec![]))
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("filePaths"));
        // 超批量上限 (显式 fileMetaMaxBatch=2) → 400
        let mut body = meta_body(vec!["a", "b", "c"]);
        body.0.file_meta_max_batch = Some(2);
        let err = get_file_meta(State(state), body)
            .await
            .err()
            .expect("should reject");
        assert!(err.to_string().contains("exceeds limit"));
    }

    #[tokio::test]
    async fn get_file_meta_missing_root_returns_empty_metas() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        // 不 seed → 工作区不存在 → 空 metas (对齐 getFileList 空列表语义)
        let res = get_file_meta(State(state), meta_body(vec!["a.txt"]))
            .await
            .expect("meta ok");
        assert_eq!(res.0["success"], json!(true));
        assert_eq!(res.0["metas"], json!([]));
    }
}

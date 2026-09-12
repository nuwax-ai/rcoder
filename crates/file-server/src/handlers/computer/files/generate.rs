//! generate-file handler: JSON 文本生成文件。

use axum::extract::State;
use garde::Validate;
use serde_json::Value;

use crate::ops::files::generate_file_impl;

use super::super::resolve_computer_target;
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::GenerateFileBody;

/// 生成文本文件
///
/// 对齐 nuwax computer generateFile, commit 9bea35e。
///
/// 与 [`super::upload::upload_file`] 区别：本接口接收 **JSON 文本** (`fileName` + `content`) 而非 multipart 上传，
/// 适用于 agent 直接由文本内容生成文件。复用 [`resolve_computer_target`] + [`path_safety::ensure_within`]
/// (路径穿越校验) + 父目录自动创建。
#[utoipa::path(
    post,
    path = "/generate-file",
    request_body = GenerateFileBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn generate_file(
    State(state): State<AppState>,
    Json(body): Json<GenerateFileBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let ws = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
        body.workspace_path.as_deref(),
    )
    .await?;
    generate_file_impl(ws, body.file_name.trim(), body.content.unwrap_or_default()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

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
            started_at: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn generate_file_writes_content_and_creates_subdirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "src/a.txt".into(),
            content: Some("hi".into()),
            custom_target_dir: None,
            workspace_path: None,
        };
        let res = generate_file(State(state), Json(body))
            .await
            .expect("generate-file should succeed");
        let val = res.0;
        assert_eq!(val["success"], serde_json::json!(true));
        assert_eq!(val["message"], "File generated successfully");
        assert_eq!(val["fileName"], "src/a.txt");
        assert_eq!(val["fileSize"], 2);
        let written = std::fs::read(computer_root.join("u").join("c").join("src").join("a.txt"))
            .expect("file written");
        assert_eq!(written, b"hi");
    }

    #[tokio::test]
    async fn generate_file_custom_target_dir_overrides_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let custom = tmp.path().join("custom-target");
        let state = make_state(tmp.path().join("c"));
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "top.txt".into(),
            content: Some("x".into()),
            custom_target_dir: Some(custom.to_string_lossy().into_owned()),
            workspace_path: None,
        };
        generate_file(State(state), Json(body))
            .await
            .expect("generate-file with customTargetDir");
        assert_eq!(
            std::fs::read(custom.join("top.txt")).expect("file at customTargetDir"),
            b"x"
        );
    }

    #[tokio::test]
    async fn generate_file_rejects_path_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "../escape.txt".into(),
            content: Some("pwned".into()),
            custom_target_dir: None,
            workspace_path: None,
        };
        let err = generate_file(State(state), Json(body))
            .await
            .err()
            .expect("path traversal must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("not safe") || msg.contains("exceed"),
            "expected path-safety error, got: {msg}"
        );
        // 确保逃逸文件未落地
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn generate_file_rejects_empty_file_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        // 对齐 TS generateFile: 空判断只看 trim 后是否为空 (whitespace-only 也算空)
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "   ".into(),
            content: None,
            custom_target_dir: None,
            workspace_path: None,
        };
        let err = generate_file(State(state), Json(body))
            .await
            .err()
            .expect("empty fileName must be rejected");
        assert!(
            err.to_string().contains("cannot be empty"),
            "expected empty-fileName error, got: {err}"
        );
    }

    #[tokio::test]
    async fn generate_file_strips_leading_slash_for_path_but_echoes_trimmed() {
        // 对齐 TS: fileName="/src/a.txt" → 写入 ws/src/a.txt (剥前导 /),
        // 但响应 fileName 回显 trim 后的 "/src/a.txt" (generateFile 返回 normalizedFileName)。
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "/src/a.txt".into(),
            content: Some("hi".into()),
            custom_target_dir: None,
            workspace_path: None,
        };
        let res = generate_file(State(state), Json(body))
            .await
            .expect("leading-slash fileName should succeed");
        let val = res.0;
        assert_eq!(val["fileName"], "/src/a.txt"); // 回显 trim 后(保留斜杠)
        assert_eq!(val["fileSize"], 2);
        // 实际写入剥前导斜杠
        let written = std::fs::read(computer_root.join("u").join("c").join("src").join("a.txt"))
            .expect("file written under src/");
        assert_eq!(written, b"hi");
    }

    // ── 项目绑定目录通道 (对齐 TS f979df7): body 字段 + x-workspace-path header ─────

    /// body 的 workspacePath 字段 → 文件落绑定目录。
    #[tokio::test]
    async fn generate_file_writes_into_body_workspace_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        let bound = tmp.path().join("bound-ws");

        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "bound.txt".into(),
            content: Some("x".into()),
            custom_target_dir: None,
            workspace_path: Some(bound.to_string_lossy().into_owned()),
        };
        generate_file(State(state), Json(body))
            .await
            .expect("generate ok");
        // 文件落在绑定目录 (generate_file_impl 内部创建父目录)
        let content = tokio::fs::read_to_string(bound.join("bound.txt"))
            .await
            .expect("file in bound dir");
        assert_eq!(content, "x");
    }

    /// x-workspace-path header 通道端到端: 中间件 scope → task-local → 收口合并。
    /// 验证 header 优先级与 wire 契约 (Java 服务间调用走 header)。
    #[tokio::test]
    async fn generate_file_honors_workspace_path_header() {
        use crate::extract::scope_workspace_path;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        let bound = tmp.path().join("header-bound-ws");

        let app = axum::Router::new()
            .route("/generate-file", axum::routing::post(generate_file))
            .layer(axum::middleware::from_fn(scope_workspace_path))
            .with_state(state);

        use tower::ServiceExt;
        let body = serde_json::json!({
            "userId": "u",
            "cId": "c",
            "fileName": "from-header.txt",
            "content": "h",
        });
        let resp = app
            .oneshot(
                axum::http::Request::post("/generate-file")
                    .header("content-type", "application/json")
                    .header("x-workspace-path", bound.to_string_lossy().as_ref())
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let content = tokio::fs::read_to_string(bound.join("from-header.txt"))
            .await
            .expect("file in header-bound dir");
        assert_eq!(content, "h");
    }
}

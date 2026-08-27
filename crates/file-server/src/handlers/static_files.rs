//! 静态文件 HTTP handlers (对齐 nuwax `server.js` 顶层 `/api/page/static` + `/api/computer/static`)。
//!
//! CORS 配置族 / serve_from_root / 响应辅助在 [`crate::ops::static_share`]
//! （userapp static 取包跨 crate 复用同层）。
//!
//! 复刻要点 (nuwax 用 `res.sendFile`/`send` 库):
//! - 根目录: page = `PROJECT_SOURCE_DIR/{projectId}`; computer = `COMPUTER_WORKSPACE_DIR/{userId}/{cId}`
//!   (或 `?customTargetDir=` 完全覆盖根)
//! - 路径循环 `decodeURIComponent` 直到稳定 (safeDecodePath)
//! - dotfiles: allow
//! - CORS: 回显 Origin (无则 `*`), Origin 存在时附 Credentials+Vary; 两套路由 Allow/Expose 头不同
//! - OPTIONS 预检 → 200 空 body
//! - 文件不存在 → 404 `Not Found`
//! - Range/ETag/Last-Modified: 由 `tower_http::services::ServeFile` 处理

use std::path::PathBuf;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::AppState;
use crate::extract::{AppPath as AxumPath, AppQuery as Query};
use crate::ops::static_share::{COMPUTER_CORS, PAGE_CORS, cors_404, serve_from_root};
use crate::workspace::ProjectContext;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomTargetQuery {
    #[serde(default)]
    custom_target_dir: Option<String>,
}

// ── page static ────────────────────────────────────────────────────────────────

/// 项目静态文件服务
#[utoipa::path(
    get,
    path = "/static/{project_id}/{*rest}",
    params(
        ("project_id" = String, Path, description = "Project identifier"),
        ("rest" = String, Path, description = "Project-relative file path")
    ),
    description = r#"
以 HTTP 直读项目工作区内任意文件（构建产物预览、前端页面直挂场景）：
按 `project_id` 定位项目根，`rest` 为根相对路径。带 CORS 头（浏览器跨源
直连 60000 场景）；`index.html` 目录补全由 rest 显式给出。
"#,
    responses(
        (status = 200, description = "Static file", body = crate::models::BinaryFile, content_type = "application/octet-stream"),
        (status = 404, description = "File not found")
    ),
    tag = "Static"
)]
pub(crate) async fn serve_page(
    State(state): State<AppState>,
    AxumPath((project_id, rest)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    if project_id.trim().is_empty() {
        return cors_404(&req, &PAGE_CORS);
    }
    let root = match state
        .resolver
        .resolve_project(&ProjectContext {
            project_id: project_id.to_string(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        })
        .await
    {
        Ok(root) => root,
        Err(error) => return error.into_response(),
    };
    serve_from_root(&root, &rest, &PAGE_CORS, req).await
}

// ── userapp static ─────────────────────────────────────────────────────────────
// serve_userapp/latest_build_artifact 已迁至 file-server-userapp crate
// （handlers/static_files.rs）——userApp 域整体拆分（洋葱模型）。

// ── computer static ────────────────────────────────────────────────────────────

/// computer 工作区静态文件服务
#[utoipa::path(
    get,
    path = "/static/{user_id}/{c_id}/{*rest}",
    params(
        ("user_id" = String, Path, description = "User identifier"),
        ("c_id" = String, Path, description = "Computer workspace identifier"),
        ("rest" = String, Path, description = "Workspace-relative file path"),
        ("customTargetDir" = Option<String>, Query, description = "Override workspace root")
    ),
    description = r#"
以 HTTP 直读 computer 树（两级 `{root}/{user_id}/{cId}` Electron 全局根语义）
内任意文件。`customTargetDir` 可覆盖根目录（默认 user/cid 推导）。同 page
static 一致：二进制原样 + CORS，404 = 路径不存在或越界。
"#,
    responses(
        (status = 200, description = "Static file", body = crate::models::BinaryFile, content_type = "application/octet-stream"),
        (status = 404, description = "File not found")
    ),
    tag = "Static"
)]
pub(crate) async fn serve_computer(
    State(state): State<AppState>,
    AxumPath((user_id, c_id, rest)): AxumPath<(String, String, String)>,
    Query(q): Query<CustomTargetQuery>,
    req: Request,
) -> Response {
    if user_id.trim().is_empty() || c_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    // userApp 分流与 computer 定位经公共收口（与 ws_path 单头，防两处漂移）
    let default_root =
        match super::computer::computer_root_for_request(&state, &user_id, &c_id).await {
            Ok(root) => root,
            Err(error) => return error.into_response(),
        };
    // customTargetDir 非空 → 完全覆盖根 (对齐 nuwax, 不拼 user/cId);
    // 注: 不做根目录白名单限制 —— 容器内内网部署, 且用户客户端复用本模块逻辑,
    // 每个用户电脑上的路径各不相同, 限制根路径会误伤正常业务。
    let root = match q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ct) => PathBuf::from(ct),
        None => default_root,
    };
    serve_from_root(&root, &rest, &COMPUTER_CORS, req).await
}

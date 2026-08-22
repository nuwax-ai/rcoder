//! userApp 开发域终端/桌面代理的文档接口（`/proxy/userapp/{ttyd,vnc,audio,ime}/{app_id}`）。
//!
//! 纯 OpenAPI 文档接口（同 `proxy_to_app/devapp_with_path` 先例）：实际流量由
//! Pingora 代理服务处理（容器 8088，K8s NodePort 30435），不经过 rcoder 主服务。
//! 本组接口让 Java 同事在 Swagger 里看到完整的对接说明——按 **app_id** 定位
//! UserAppBuilder 开发容器（与 computer 族按 user_id 定位沙箱对称）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::handler::ProxyErrorResponse;
use crate::router::AppState;

use chrono::Utc;

/// 公共：构造 307 重定向到 Pingora 的文档化响应（或 503 说明）。
async fn redirect_doc_response(
    state: &AppState,
    kind: &str,
    app_id: String,
    path: String,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    let Some(proxy_config) = state.config.proxy_config.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    };
    let listen_port = proxy_config.listen_port;
    let target_path = if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path)
    };
    let location =
        format!("http://127.0.0.1:{listen_port}/proxy/userapp/{kind}/{app_id}{target_path}");
    axum::http::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(axum::http::header::LOCATION, location)
        .body(axum::body::Body::empty())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProxyErrorResponse {
                    error: "RESPONSE_BUILD_ERROR".to_string(),
                    message: format!("failed to build response: {e}"),
                    target_port: 0,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })
}

/// Pingora 代理 - userApp 开发域 ttyd 终端
#[utoipa::path(
    get,
    path = "/proxy/userapp/ttyd/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域 Web 终端（ttyd，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **Web 终端**。与 computer 族
（`/computer/ttyd/{user_id}/...` 按用户沙箱）对称的开发场景入口，按 **app_id** 定位。

- upstream 经容器内 agent_runner 的 ws_terminal 中间层（17681）连接 ttyd 本体（7681）；
  客户端 WebSocket 须携带子协议 `tty`。
- **终端工作目录 = 开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`**（Pingora 注入
  `X-Ttyd-Service-Type: user-app-builder`，agent_runner 据此定位 cwd——与 chat
  开发对话、file-server 的 workspace 同根）。
- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（rcoder 容器 8088 / K8s NodePort 30435），须直连 Pingora 不走 rcoder 主端口。

> 例：`GET /proxy/userapp/ttyd/app-order-svc/`（终端页面）；WebSocket `/proxy/userapp/ttyd/app-order-svc/ws`。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器；workspace 须已创建）"),
        ("path" = String, Path, description = "ttyd 内路径（`/` 终端页面；`ws` WebSocket）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建（先调 POST /api/userapp/workspace）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_ttyd(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "ttyd", app_id, path).await
}

/// Pingora 代理 - userApp 开发域远程桌面（noVNC）
#[utoipa::path(
    get,
    path = "/proxy/userapp/vnc/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域远程桌面（noVNC，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **远程桌面**（noVNC）。开发容器是完整桌面镜像
（Xvnc 5900 + noVNC 6080），与 computer 族 `/computer/vnc/{user_id}/...` 对称。

- upstream = 容器内 noVNC（6080，HTTP 页面 + `websockify` WebSocket 同端口）。
- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /proxy/userapp/vnc/app-order-svc/vnc.html`（桌面页面）；WebSocket `/proxy/userapp/vnc/app-order-svc/websockify`。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器）"),
        ("path" = String, Path, description = "noVNC 内路径（`vnc.html` 页面；`websockify` WebSocket）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_vnc(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "vnc", app_id, path).await
}

/// Pingora 代理 - userApp 开发域语音（audio）
#[utoipa::path(
    get,
    path = "/proxy/userapp/audio/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域语音（audio，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **语音服务**。分流规则与 computer 族
`/computer/audio/{user_id}/...` 一致：

- `path` 为 `ws` 或 `ws/*` → WebSocket 语音流（6089）
- 其余（含空路径）→ HTTP 静态资源/播放器页面（6090）

- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /proxy/userapp/audio/app-order-svc/`（播放器页面）；WebSocket `/proxy/userapp/audio/app-order-svc/ws`。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器）"),
        ("path" = String, Path, description = "`ws`/`ws/*` 走 6089 流；其余走 6090 静态")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_audio(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "audio", app_id, path).await
}

/// Pingora 代理 - userApp 开发域输入法（IME）
#[utoipa::path(
    get,
    path = "/proxy/userapp/ime/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域输入法（IME，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **IME 输入法透传服务**（6091，WebSocket）。
客户端本地输入法经 WebSocket 发送文本，容器内用 xdotool 输入到远程桌面——
与 computer 族 `/computer/ime/{user_id}/...` 对称。

- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`WebSocket /proxy/userapp/ime/app-order-svc/connect`。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器）"),
        ("path" = String, Path, description = "`connect` 为 WebSocket 连接端点")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_ime(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "ime", app_id, path).await
}

/// 开发域终端代理入口一览（文档辅助接口：Java 拼接 URL 的速查表）。
#[utoipa::path(
    get,
    path = "/proxy/userapp/routes",
    tag = "应用管理",
    summary = "userApp 开发域代理路由一览（终端/桌面/语音/输入法/端口预览）",
    description = "userApp 开发阶段的 Pingora 代理入口速查（全部按 app_id 定位 UserAppBuilder 开发容器，前置 POST /api/userapp/workspace）。",
    responses(
        (status = 200, description = "路由清单", body = Value)
    )
)]
pub async fn userapp_proxy_routes_doc() -> Json<Value> {
    Json(json!({
        "terminal": {
            "ttyd（Web 终端，cwd=开发卷/{app_id}）": "/proxy/userapp/ttyd/{app_id}/{path}",
            "ws 子协议": "tty", "upstream": "ws_terminal(17681) → ttyd(7681)"
        },
        "desktop": {
            "vnc（noVNC 远程桌面）": "/proxy/userapp/vnc/{app_id}/{path}",
            "upstream": "noVNC(6080, HTTP+websockify WS)"
        },
        "audio": {
            "语音": "/proxy/userapp/audio/{app_id}/{path}",
            "分流": "ws* → 6089 流；其余 → 6090 静态"
        },
        "ime": {
            "输入法": "/proxy/userapp/ime/{app_id}/{path}",
            "upstream": "IME(6091, WebSocket connect)"
        },
        "portPreview": {
            "devapps（任意 dev 端口预览）": "/proxy/devapps/{user_id}/{app_id}/{port}/{path}",
            "说明": "基础设施端口(5432/60000/8086/50051/6080/17681/7681/6089-6091) 已封禁，终端/桌面请走上表专用入口"
        },
        "entry": "Pingora 入口 = rcoder 容器 8088 / K8s NodePort 30435（不经 rcoder 主端口）"
    }))
}

// ── 根路径（无尾随 path）变体：/proxy/userapp/{kind}/{app_id} → 同款 307（path 置空） ──

macro_rules! root_redirect {
    ($fn_name:ident, $kind:expr) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            Path(app_id): Path<String>,
        ) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
            redirect_doc_response(&state, $kind, app_id, String::new()).await
        }
    };
}

root_redirect!(proxy_to_userapp_ttyd_redirect_root, "ttyd");
root_redirect!(proxy_to_userapp_vnc_redirect_root, "vnc");
root_redirect!(proxy_to_userapp_audio_redirect_root, "audio");
root_redirect!(proxy_to_userapp_ime_redirect_root, "ime");

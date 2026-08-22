//! userApp 终端/桌面代理的文档接口。
//!
//! - **开发域**：`/userapp/{ttyd,vnc,audio,ime}/{app_id}`——按 app_id 定位
//!   UserAppBuilder 开发容器（与 computer 族按 user_id 定位沙箱对称）。
//! - **运行域**：`/userapp/{ttyd,pgweb}/{app_id}/runtime`——按 app_id 定位
//!   `ServiceType::UserApp` 运行容器（部署后的生产环境，线上排障）。
//!
//! 纯 OpenAPI 文档接口（同 `proxy_to_app/devapp_with_path` 先例）：实际流量由
//! Pingora 代理服务处理（容器 8088，K8s NodePort 30435），不经过 rcoder 主服务。
//! 本组接口让 Java 同事在 Swagger 里看到完整的对接说明。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::handler::ProxyErrorResponse;
use crate::router::AppState;

use chrono::Utc;

/// 公共：构造 307 重定向到 Pingora 的文档化响应（或 503 说明）。
///
/// `suffix`：app_id 后的静态段（开发域=""；运行容器="/runtime"）。
async fn redirect_doc_response(
    state: &AppState,
    kind: &str,
    app_id: String,
    suffix: &str,
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
        format!("http://127.0.0.1:{listen_port}/userapp/{kind}/{app_id}{suffix}{target_path}");
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
    path = "/userapp/ttyd/{app_id}/{*path}",
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

> 例：`GET /userapp/ttyd/app-order-svc/`（终端页面）；WebSocket `/userapp/ttyd/app-order-svc/ws`。
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
    redirect_doc_response(&state, "ttyd", app_id, "", path).await
}

/// Pingora 代理 - userApp 开发域远程桌面（noVNC）
#[utoipa::path(
    get,
    path = "/userapp/vnc/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域远程桌面（noVNC，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **远程桌面**（noVNC）。开发容器是完整桌面镜像
（Xvnc 5900 + noVNC 6080），与 computer 族 `/computer/vnc/{user_id}/...` 对称。

- upstream = 容器内 noVNC（6080，HTTP 页面 + `websockify` WebSocket 同端口）。
- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /userapp/vnc/app-order-svc/vnc.html`（桌面页面）；WebSocket `/userapp/vnc/app-order-svc/websockify`。
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
    redirect_doc_response(&state, "vnc", app_id, "", path).await
}

/// Pingora 代理 - userApp 开发域语音（audio）
#[utoipa::path(
    get,
    path = "/userapp/audio/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域语音（audio，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **语音服务**。分流规则与 computer 族
`/computer/audio/{user_id}/...` 一致：

- `path` 为 `ws` 或 `ws/*` → WebSocket 语音流（6089）
- 其余（含空路径）→ HTTP 静态资源/播放器页面（6090）

- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /userapp/audio/app-order-svc/`（播放器页面）；WebSocket `/userapp/audio/app-order-svc/ws`。
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
    redirect_doc_response(&state, "audio", app_id, "", path).await
}

/// Pingora 代理 - userApp 开发域输入法（IME）
#[utoipa::path(
    get,
    path = "/userapp/ime/{app_id}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 开发域输入法（IME，按 app_id 定位开发容器）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **IME 输入法透传服务**（6091，WebSocket）。
客户端本地输入法经 WebSocket 发送文本，容器内用 xdotool 输入到远程桌面——
与 computer 族 `/computer/ime/{user_id}/...` 对称。

- 前置：`POST /api/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`WebSocket /userapp/ime/app-order-svc/connect`。
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
    redirect_doc_response(&state, "ime", app_id, "", path).await
}

/// 开发域终端代理入口一览（文档辅助接口：Java 拼接 URL 的速查表）。
#[utoipa::path(
    get,
    path = "/userapp/routes",
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
            "ttyd（Web 终端，cwd=开发卷/{app_id}）": "/userapp/ttyd/{app_id}/{path}",
            "ws 子协议": "tty", "upstream": "ws_terminal(17681) → ttyd(7681)"
        },
        "desktop": {
            "vnc（noVNC 远程桌面）": "/userapp/vnc/{app_id}/{path}",
            "upstream": "noVNC(6080, HTTP+websockify WS)"
        },
        "audio": {
            "语音": "/userapp/audio/{app_id}/{path}",
            "分流": "ws* → 6089 流；其余 → 6090 静态"
        },
        "ime": {
            "输入法": "/userapp/ime/{app_id}/{path}",
            "upstream": "IME(6091, WebSocket connect)"
        },
        "runtime": {
            "说明": "部署后的生产环境（ServiceType::UserApp 运行容器，app-runtime 镜像）——线上排障入口；未部署/已停止 → 502",
            "ttyd（Web 终端，cwd=/app，直连 ttyd 本体不经 ws_terminal）": "/userapp/ttyd/{app_id}/runtime/{path}",
            "pgweb（容器内 PG 的 Web 控制台）": "/userapp/pgweb/{app_id}/runtime/{path}"
        },
        "portPreview": {
            "devapps（任意 dev 端口预览）": "/proxy/devapps/{user_id}/{app_id}/{port}/{path}",
            "说明": "基础设施端口(5432/60000/8086/50051/6080/17681/7681/6089-6091) 已封禁，终端/桌面请走上表专用入口"
        },
        "entry": "Pingora 入口 = rcoder 容器 8088 / K8s NodePort 30435（不经 rcoder 主端口）"
    }))
}

// ── 根路径（无尾随 path）变体：/userapp/{kind}/{app_id} → 同款 307（path 置空） ──

macro_rules! root_redirect {
    ($fn_name:ident, $kind:expr) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            Path(app_id): Path<String>,
        ) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
            redirect_doc_response(&state, $kind, app_id, "", String::new()).await
        }
    };
}

root_redirect!(proxy_to_userapp_ttyd_redirect_root, "ttyd");
root_redirect!(proxy_to_userapp_vnc_redirect_root, "vnc");
root_redirect!(proxy_to_userapp_audio_redirect_root, "audio");
root_redirect!(proxy_to_userapp_ime_redirect_root, "ime");

// ── 运行容器（部署后的生产环境）：/userapp/{ttyd,pgweb}/{app_id}/runtime/{*path} ──

/// Pingora 代理 - userApp 运行容器 Web 终端（ttyd）
#[utoipa::path(
    get,
    path = "/userapp/ttyd/{app_id}/runtime/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 运行容器 Web 终端（部署后的生产环境，ttyd 直连）",
    description = r#"
访问该 app **运行容器**（`ServiceType::UserApp`，app-runtime 镜像——部署后的生产环境）
内的 Web 终端，供线上排障。与开发域 `/userapp/ttyd/{app_id}`（UserAppBuilder 开发容器、
经 ws_terminal 中间层定位到开发卷 cwd）的关键差异：

- upstream **直连 ttyd 本体**（7681，WebSocket）——运行容器没有 agent_runner，
  不经 ws_terminal(17681) 中间层；客户端 WebSocket 须携带子协议 `tty`。
- 终端工作目录 = 容器内 `/app`（镜像 supervisor 固定 `ttyd -w /app`）。
- 定位按确定性 Service 名（K8s Pod 重建 DNS 自愈）；**app 未部署/已停止 → 上游
  连接失败 502**（区别于开发域未注册 404）。

> 例：`GET /userapp/ttyd/app-order-svc/runtime/`；WebSocket `/userapp/ttyd/app-order-svc/runtime/ws`。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其运行容器）"),
        ("path" = String, Path, description = "ttyd 内路径（`/` 终端页面；`ws` WebSocket）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 502, description = "app 未部署或已停止（运行容器不可达）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_runtime_ttyd(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "ttyd", app_id, "/runtime", path).await
}

/// Pingora 代理 - userApp 运行容器数据库控制台（pgweb）
#[utoipa::path(
    get,
    path = "/userapp/pgweb/{app_id}/runtime/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - userApp 运行容器数据库 Web 控制台（pgweb，部署后的生产环境）",
    description = r#"
访问该 app **运行容器**（app-runtime 镜像）内的 pgweb——容器内 PostgreSQL（5432）
的 Web 控制台（8081，普通 HTTP），供线上查库排障。

- 容器内 PG 由 supervisor 恒起（`/app/data/pg` 持久于 app 的 RWX PVC）；
  pgweb 会话直连容器内本实例。
- **app 未部署/已停止 → 上游连接失败 502**。

> 例：`GET /userapp/pgweb/app-order-svc/runtime/`（控制台页面）。
"#,
    params(
        ("app_id" = String, Path, description = "应用 ID（定位其运行容器）"),
        ("path" = String, Path, description = "pgweb 内路径（`/` 控制台页面）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 502, description = "app 未部署或已停止（运行容器不可达）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_runtime_pgweb(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "pgweb", app_id, "/runtime", path).await
}

/// 运行容器代理根路径变体（无尾随 path → 同款 307）。
macro_rules! runtime_root_redirect {
    ($fn_name:ident, $kind:expr) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            Path(app_id): Path<String>,
        ) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
            redirect_doc_response(&state, $kind, app_id, "/runtime", String::new()).await
        }
    };
}

runtime_root_redirect!(proxy_to_userapp_runtime_ttyd_redirect_root, "ttyd");
runtime_root_redirect!(proxy_to_userapp_runtime_pgweb_redirect_root, "pgweb");

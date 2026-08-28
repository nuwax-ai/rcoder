//! userApp 工具族代理的文档接口。
//!
//! - **开发域工具族**：`/userapp/dev/{ttyd,vnc,audio,ime,dbx}/{user_id}/{app_id}`——
//!   定位 UserAppBuilder 开发容器；user_id 是懒创建显式 owner 档（dev/{user_id}/{app_id}
//!   宿主树分区）+ 审计锚点（与 computer 族按 user_id 定位沙箱对称）。
//! - **生产域工具族**：`/userapp/prod/{ttyd,pgweb,dbx}/{user_id}/{app_id}`——定位
//!   `ServiceType::UserApp` 运行容器；user_id 为归属校验锚点（容器不在时配合唤醒）。
//! - **应用流量族**（免端口）：`/proxy/userapp/{dev,prod}/{user_id}/{app_id}`——
//!   见 `proxy_handler_api` 的 `proxy_to_app/devapp_with_path`。
//!
//! stage 段 dev/prod 全族统一（前端切环境只改一段）；实际流量由 Pingora 代理服务
//! 处理（容器 8088，K8s NodePort 30435），不经过 rcoder 主服务。本组接口让 Java
//! 同事在 Swagger 里看到完整的对接说明。

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
/// Pingora 侧路由前缀 = `/userapp/{stage}/{tool}`（stage 段 dev/prod 区分定位方式）。
async fn redirect_doc_response(
    state: &AppState,
    stage: &str,
    tool: &str,
    user_id: String,
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
    let location = format!(
        "http://127.0.0.1:{listen_port}/userapp/{stage}/{tool}/{user_id}/{app_id}{target_path}"
    );
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

// ── 开发域工具族：/userapp/dev/{ttyd,vnc,audio,ime,dbx}/{app_id}/{*path} ──

/// Pingora 代理 - userApp 开发域 ttyd 终端
#[utoipa::path(
    get,
    path = "/userapp/dev/ttyd/{user_id}/{app_id}/{*path}",
    tag = "UserApp · dev · 终端工具",
    summary = "开发容器 Web 终端（ttyd）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **Web 终端**。与 computer 族
（`/computer/ttyd/{user_id}/...` 按用户沙箱）对称的开发场景入口，按 **app_id** 定位。

- upstream 经容器内 agent_runner 的 ws_terminal 中间层（17681）连接 ttyd 本体（7681）；
  客户端 WebSocket 须携带子协议 `tty`。
- **终端工作目录 = 开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`**（Pingora 注入
  `X-Ttyd-Service-Type: user-app-builder`，agent_runner 据此定位 cwd——与 chat
  开发对话、file-server 的 workspace 同根）。
- 前置：`POST /api/v1/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（rcoder 容器 8088 / K8s NodePort 30435），须直连 Pingora 不走 rcoder 主端口。

> 例：`GET /userapp/dev/ttyd/u1/app-order-svc/`（终端页面）；WebSocket `/userapp/dev/ttyd/u1/app-order-svc/ws`。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器；workspace 须已创建）"),
        ("path" = String, Path, description = "ttyd 内路径（`/` 终端页面；`ws` WebSocket）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建（先调 POST /api/v1/userapp/workspace）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_userapp_ttyd(
    State(state): State<Arc<AppState>>,
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "dev", "ttyd", user_id, app_id, path).await
}

/// Pingora 代理 - userApp 开发域远程桌面（noVNC）
#[utoipa::path(
    get,
    path = "/userapp/dev/vnc/{user_id}/{app_id}/{*path}",
    tag = "UserApp · dev · 终端工具",
    summary = "开发容器远程桌面（noVNC）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **远程桌面**（noVNC）。开发容器是完整桌面镜像
（Xvnc 5900 + noVNC 6080），与 computer 族 `/computer/vnc/{user_id}/...` 对称。

- upstream = 容器内 noVNC（6080，HTTP 页面 + `websockify` WebSocket 同端口）。
- 前置：`POST /api/v1/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /userapp/dev/vnc/u1/app-order-svc/vnc.html`（桌面页面）；WebSocket `/userapp/dev/vnc/u1/app-order-svc/websockify`。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
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
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "dev", "vnc", user_id, app_id, path).await
}

/// Pingora 代理 - userApp 开发域语音（audio）
#[utoipa::path(
    get,
    path = "/userapp/dev/audio/{user_id}/{app_id}/{*path}",
    tag = "UserApp · dev · 终端工具",
    summary = "开发容器语音代理（audio）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **语音服务**。分流规则与 computer 族
`/computer/audio/{user_id}/...` 一致：

- `path` 为 `ws` 或 `ws/*` → WebSocket 语音流（6089）
- 其余（含空路径）→ HTTP 静态资源/播放器页面（6090）

- 前置：`POST /api/v1/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`GET /userapp/dev/audio/u1/app-order-svc/`（播放器页面）；WebSocket `/userapp/dev/audio/u1/app-order-svc/ws`。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
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
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "dev", "audio", user_id, app_id, path).await
}

/// Pingora 代理 - userApp 开发域输入法（IME）
#[utoipa::path(
    get,
    path = "/userapp/dev/ime/{user_id}/{app_id}/{*path}",
    tag = "UserApp · dev · 终端工具",
    summary = "开发容器输入法代理（IME）",
    description = r#"
访问该 app 开发容器（UserAppBuilder）内的 **IME 输入法透传服务**（6091，WebSocket）。
客户端本地输入法经 WebSocket 发送文本，容器内用 xdotool 输入到远程桌面——
与 computer 族 `/computer/ime/{user_id}/...` 对称。

- 前置：`POST /api/v1/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- host = Pingora 入口（8088 / K8s NodePort 30435）。

> 例：`WebSocket /userapp/dev/ime/u1/app-order-svc/connect`。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
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
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "dev", "ime", user_id, app_id, path).await
}

/// Pingora 代理 - DBX 数据库 Web GUI（开发阶段，UserAppBuilder 开发容器）
#[utoipa::path(
    get,
    path = "/userapp/dev/dbx/{user_id}/{app_id}/{*path}",
    tag = "UserApp · dev · 终端工具",
    summary = "开发容器数据库控制台（DBX）",
    description = r#"
访问该 app **开发容器**（UserAppBuilder，agent-runner 镜像）内的 DBX 数据库
Web GUI（60+ 数据库，supervisor 恒起 4224）——开发阶段查库/改数据的全功能控制台
（容器内 PG 已预置连接 + 首访浏览器设密码；也可连远端库）。

- 与 pgweb（prod 域专用、只读排障向）互补：DBX 是两阶段全功能 GUI。
- 前置：`POST /api/v1/userapp/workspace` 已创建该 app 的开发容器；未注册 → 404。
- 代理剥前缀直连 root 模式 dbx（前端运行时自推断 base path，API/WS 自动拼回本前缀）。
- host = Pingora 入口（rcoder 容器 8088 / K8s NodePort 30435），须直连 Pingora 不走 rcoder 主端口。

> 例：`GET /userapp/dev/dbx/u1/app-order-svc/`（DBX 控制台页面）。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器；workspace 须已创建）"),
        ("path" = String, Path, description = "dbx 内路径（`/` 控制台页面；`api/*` REST；WebSocket 透传）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 404, description = "该 app 的开发容器未创建（先调 POST /api/v1/userapp/workspace）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_dev_dbx(
    State(state): State<Arc<AppState>>,
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "dev", "dbx", user_id, app_id, path).await
}

// ── 生产域工具族（运行容器，部署后的生产环境）：/userapp/prod/{ttyd,pgweb,dbx}/{app_id}/{*path} ──

/// Pingora 代理 - userApp 运行容器 Web 终端（ttyd）
#[utoipa::path(
    get,
    path = "/userapp/prod/ttyd/{user_id}/{app_id}/{*path}",
    tag = "UserApp · prod · 终端工具",
    summary = "生产容器 Web 终端（ttyd）",
    description = r#"
访问该 app **运行容器**（`ServiceType::UserApp`，app-runtime 镜像——部署后的生产环境）
内的 Web 终端，供线上排障。与开发域 `/userapp/dev/ttyd/{app_id}`（UserAppBuilder 开发容器、
经 ws_terminal 中间层定位到开发卷 cwd）的关键差异：

- upstream **直连 ttyd 本体**（7681，WebSocket）——运行容器没有 agent_runner，
  不经 ws_terminal(17681) 中间层；客户端 WebSocket 须携带子协议 `tty`。
- 终端工作目录 = 容器内 `/app`（镜像 supervisor 固定 `ttyd -w /app`）。
- 定位按确定性 Service 名（K8s Pod 重建 DNS 自愈）；**app 未部署/已停止 → 上游
  连接失败 502**（区别于开发域未注册 404）。

> 例：`GET /userapp/prod/ttyd/u1/app-order-svc/`；WebSocket `/userapp/prod/ttyd/u1/app-order-svc/ws`。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
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
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "prod", "ttyd", user_id, app_id, path).await
}

/// Pingora 代理 - userApp 运行容器数据库控制台（pgweb）
#[utoipa::path(
    get,
    path = "/userapp/prod/pgweb/{user_id}/{app_id}/{*path}",
    tag = "UserApp · prod · 终端工具",
    summary = "生产容器数据库控制台（pgweb）",
    description = r#"
访问该 app **运行容器**（app-runtime 镜像）内的 pgweb——容器内 PostgreSQL（5432）
的 Web 控制台（8081，普通 HTTP），供线上查库排障。

- 容器内 PG 由 supervisor 恒起（`/app/data/pg` 持久于 app 的 RWX PVC）；
  pgweb 会话直连容器内本实例。
- **app 未部署/已停止 → 上游连接失败 502**。

> 例：`GET /userapp/prod/pgweb/u1/app-order-svc/`（控制台页面）。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
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
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "prod", "pgweb", user_id, app_id, path).await
}

/// Pingora 代理 - DBX 数据库 Web GUI（生产阶段，UserApp 运行容器）
#[utoipa::path(
    get,
    path = "/userapp/prod/dbx/{user_id}/{app_id}/{*path}",
    tag = "UserApp · prod · 终端工具",
    summary = "生产容器数据库控制台（DBX）",
    description = r#"
访问该 app **运行容器**（`ServiceType::UserApp`，app-runtime 镜像——部署后的生产环境）
内的 DBX 数据库 Web GUI（supervisor 恒起 4224），供线上查库排障。

- 容器内 PG 由 supervisor 恒起（`/app/data/pg` 持久于 app 的 RWX PVC）；
  dbx 已预置该连接（首访浏览器设密码，存容器内 dbx.db）。
- **app 未部署/已停止 → 上游连接失败 502**（区别于 dev 阶段未注册 404）。
- host = Pingora 入口（rcoder 容器 8088 / K8s NodePort 30435）。

> 例：`GET /userapp/prod/dbx/u1/app-order-svc/`（DBX 控制台页面）。
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（dev 懒创建显式 owner 档 + 审计锚点；prod 归属校验锚点）"),
        ("app_id" = String, Path, description = "应用 ID（定位其运行容器）"),
        ("path" = String, Path, description = "dbx 内路径（`/` 控制台页面）")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 502, description = "app 未部署或已停止（运行容器不可达）", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_prod_dbx(
    State(state): State<Arc<AppState>>,
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    redirect_doc_response(&state, "prod", "dbx", user_id, app_id, path).await
}

/// userApp 代理入口一览（文档辅助接口：Java 拼接 URL 的速查表）。
#[utoipa::path(
    get,
    path = "/userapp/routes",
    tag = "UserApp · 访问入口",
    summary = "userApp 代理路由一览",
    description = "userApp 两族 Pingora 代理入口速查：工具族 /userapp/{dev,prod}/{tool}/{user_id}/{app_id}；应用流量族（免端口，pingap 统一入口 9080）/proxy/userapp/{dev,prod}/{user_id}/{app_id}。",
    responses(
        (status = 200, description = "路由清单", body = Value)
    )
)]
pub async fn userapp_proxy_routes_doc() -> Json<Value> {
    Json(json!({
        "tools": {
            "说明": "容器内固定控制台（supervisor 恒起）；user_id=归属（dev 懒创建显式 owner 档 / prod 归属锚点）；stage 段 dev=开发容器(UserAppBuilder，未建 workspace → 404) / prod=运行容器(未部署 → 502)",
            "dev": {
                "ttyd（Web 终端，cwd=开发卷/{app_id}，ws 子协议 tty）": "/userapp/dev/ttyd/{user_id}/{app_id}/{path}",
                "vnc（noVNC 远程桌面）": "/userapp/dev/vnc/{user_id}/{app_id}/{path}",
                "audio（ws*→6089 流；其余→6090 静态）": "/userapp/dev/audio/{user_id}/{app_id}/{path}",
                "ime（输入法，WebSocket connect）": "/userapp/dev/ime/{user_id}/{app_id}/{path}",
                "dbx（数据库 Web GUI，已预置本地 PG 连接，首访设密码）": "/userapp/dev/dbx/{user_id}/{app_id}/{path}"
            },
            "prod": {
                "ttyd（Web 终端，cwd=/app，直连 ttyd 本体不经 ws_terminal）": "/userapp/prod/ttyd/{user_id}/{app_id}/{path}",
                "pgweb（容器内 PG 的 Web 控制台）": "/userapp/prod/pgweb/{user_id}/{app_id}/{path}",
                "dbx（数据库 Web GUI，已预置本地 PG 连接，首访设密码）": "/userapp/prod/dbx/{user_id}/{app_id}/{path}"
            }
        },
        "app_traffic": {
            "说明": "应用流量（免端口——代理内部固定拨 pingap 统一入口 APP_ENTRY_PORT=9080）；切环境只改 dev→prod 一段；dev 无开发容器 → 502，prod 未部署 → 502",
            "dev（开发预览，manifest 流程恒起 pingap）": "/proxy/userapp/dev/{user_id}/{app_id}/{path}",
            "prod（部署访问，access.external.http 即此格式）": "/proxy/userapp/prod/{user_id}/{app_id}/{path}"
        },
        "entry": "Pingora 入口 = rcoder 容器 8088 / K8s NodePort 30435（不经 rcoder 主端口）"
    }))
}

// ── 根路径（无尾随 path）变体：/userapp/{stage}/{tool}/{app_id} → 同款 307（path 置空） ──

macro_rules! stage_tool_redirect {
    ($fn_name:ident, $stage:expr, $tool:expr) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            Path((user_id, app_id)): Path<(String, String)>,
        ) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
            redirect_doc_response(&state, $stage, $tool, user_id, app_id, String::new()).await
        }
    };
}

stage_tool_redirect!(proxy_to_userapp_ttyd_redirect_root, "dev", "ttyd");
stage_tool_redirect!(proxy_to_userapp_vnc_redirect_root, "dev", "vnc");
stage_tool_redirect!(proxy_to_userapp_audio_redirect_root, "dev", "audio");
stage_tool_redirect!(proxy_to_userapp_ime_redirect_root, "dev", "ime");
stage_tool_redirect!(proxy_to_dev_dbx_redirect_root, "dev", "dbx");
stage_tool_redirect!(proxy_to_userapp_runtime_ttyd_redirect_root, "prod", "ttyd");
stage_tool_redirect!(
    proxy_to_userapp_runtime_pgweb_redirect_root,
    "prod",
    "pgweb"
);
stage_tool_redirect!(proxy_to_prod_dbx_redirect_root, "prod", "dbx");

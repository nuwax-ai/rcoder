//! computer 桌面 ttyd 代理（从 computer_desktop_handler.rs 拆出）。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use std::sync::Arc;
use utoipa::ToSchema;

use super::super::utils::I18nPath;
use super::proxy::DesktopErrorResponse;
use crate::HttpResult;
use crate::router::AppState;

/// ttyd Web 终端代理路径参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct TtydProxyPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
    /// 剩余路径
    ///
    /// ## 路径说明
    /// - 空或 `/`: ttyd 主页面（HTTP 7681）
    /// - `ws`: WebSocket 连接端点
    /// - `ws/token`: 带 token 的 WebSocket 连接
    #[schema(example = "ws", nullable = true)]
    pub path: Option<String>,
}

/// ttyd Web 终端代理（通过 Pingora 代理）
///
/// 这是一个占位实现，用于生成 OpenAPI 文档。
/// 实际的 ttyd 代理请求会通过 Pingora 透明代理到容器的 ttyd 服务。
///
/// ## 路径说明
/// - `HTTP /computer/ttyd/{user_id}/{project_id}/` - ttyd 主页面
/// - `WebSocket /computer/ttyd/{user_id}/{project_id}/ws` - 终端 WebSocket
///
/// ## 端口说明
/// - **HTTP + WebSocket 7681**: ttyd Web 终端服务（单端口双协议）
///
/// ## 工作原理
///
/// ttyd 是一个基于 libwebsockets 的 Web 终端工具，在容器内监听端口 7681，
/// 同时提供 HTTP 页面和 WebSocket 终端连接。
///
/// 1. **HTTP 请求**: 返回 ttyd Web UI 页面（HTML + JavaScript）
/// 2. **WebSocket 请求**: 通过 `Upgrade: websocket` 头升级连接到 PTY 终端
/// 3. Pingora 透明代理所有请求（包括 WebSocket Upgrade）到容器的 7681 端口
/// 4. ttyd 端 libwebsockets 根据 Upgrade 头自动分发到 PTY 协议
///
/// ## WebSocket 子协议
///
/// 客户端连接时必须指定子协议 `tty`：
/// ```javascript
/// const ws = new WebSocket('ws://host/computer/ttyd/user_123/proj_456/ws', ['tty']);
/// ```
///
/// ## 使用示例
///
/// ```javascript
/// // 打开 ttyd Web 终端页面
/// window.open('/computer/ttyd/user_123/proj_456/', '_blank');
///
/// // 或通过 WebSocket 直接连接终端
/// const ws = new WebSocket('ws://localhost:8088/computer/ttyd/user_123/proj_456/ws', ['tty']);
/// ws.onmessage = (event) => {
///     // 接收终端输出（UTF-8 文本）
///     terminal.write(event.data);
/// };
/// ws.send('ls -la\n'); // 发送命令
/// ```
#[utoipa::path(
    get,
    path = "/computer/ttyd/{user_id}/{project_id}/{*path}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID"),
        ("path" = Option<String>, Path, description = "剩余路径（ws 表示 WebSocket 端点）")
    ),
    responses(
        (
            status = 501,
            description = "未实现：本端点由 Pingora 代理，直接调用 rcoder 返回此占位响应",
            body = DesktopErrorResponse
        ),
        (
            status = 200,
            description = "ttyd Web UI 页面（HTTP）",
            content_type = "text/html"
        ),
        (
            status = 101,
            description = "WebSocket 升级响应（终端连接）",
            body = String
        ),
        (
            status = 404,
            description = "找不到用户容器",
            body = HttpResult<DesktopErrorResponse>
        ),
        (
            status = 503,
            description = "代理服务未启用",
            body = HttpResult<DesktopErrorResponse>
        )
    ),
    tag = "computer",
    operation_id = "computer_ttyd_proxy",
    summary = "ttyd Web 终端代理",
    description = r#"
通过 Pingora 代理访问容器的 ttyd Web 终端服务。

## 访问方式

### ttyd Web UI 页面
```
GET /computer/ttyd/{user_id}/{project_id}/
```

### ttyd WebSocket 终端
```
WebSocket /computer/ttyd/{user_id}/{project_id}/ws
```

## 工作原理

1. **客户端**: 浏览器打开 ttyd 页面或建立 WebSocket 连接
2. **Pingora 代理**: 根据 user_id 查找容器 IP，代理到端口 7681
3. **ttyd 服务**: libwebsockets 根据 Upgrade 头分发到 PTY 终端
4. **终端交互**: 双向传输 UTF-8 文本数据

## 端口说明

| 协议 | 端口 | 说明 |
|------|------|------|
| HTTP | 7681 | ttyd Web UI 页面 |
| WebSocket | 7681 | 终端 WebSocket（同端口，靠 Upgrade 头区分） |

## WebSocket 子协议

连接时必须指定子协议 `tty`，否则 ttyd 不会 fork bash：
```javascript
const ws = new WebSocket('ws://host/computer/ttyd/user_123/proj_456/ws', ['tty']);
```

## 使用示例

```javascript
// 打开 Web 终端
window.open('/computer/ttyd/user_123/proj_456/', '_blank');

// 或在 iframe 中嵌入
<iframe src="/computer/ttyd/user_123/proj_456/" width="100%" height="600"></iframe>

// 直接 WebSocket 连接
const ws = new WebSocket('ws://localhost:8088/computer/ttyd/user_123/proj_456/ws', ['tty']);
ws.onmessage = (event) => {
    terminal.write(event.data); // 接收终端输出
};
ws.send('ls -la\n'); // 发送命令
```
"#
)]
pub async fn computer_ttyd_proxy(
    State(_state): State<Arc<AppState>>,
    I18nPath((user_id, project_id, path)): I18nPath<(String, String, Option<String>)>,
) -> impl IntoResponse {
    let error_response = DesktopErrorResponse {
        error: "PROXY_REDIRECT".to_string(),
        message: format!(
            "请使用 Pingora 代理路径访问 ttyd 终端，路径: /computer/ttyd/{}/{}/{}",
            user_id,
            project_id,
            path.as_deref().unwrap_or("")
        ),
        user_id,
        project_id,
    };

    (StatusCode::NOT_IMPLEMENTED, Json(error_response))
}

// ============================================================================
// 辅助函数
// ============================================================================

// ============================================================================
// 单元测试
// ============================================================================

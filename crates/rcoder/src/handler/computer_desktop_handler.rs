//! Computer Agent Runner VNC 桌面处理器
//!
//! 提供 VNC 桌面访问功能，允许用户通过 WebSocket 连接到容器内的 noVNC 服务。
//!
//! ## 端口说明
//! - noVNC WebSocket: 6080 (容器内)
//!
//! ## 实现状态
//!
//! **当前版本已实现 Pingora WebSocket 透明代理。**
//!
//! 客户端应使用 Pingora 代理路径访问 VNC：
//! - VNC 页面: `http://{proxy_host}/computer/vnc/{user_id}/{project_id}/vnc.html`
//! - WebSocket: `ws://{proxy_host}/computer/vnc/{user_id}/{project_id}/websockify`
//!
//! Pingora 会自动将请求透明代理到对应用户容器的 noVNC 服务（端口 6080）。
//!
//! ## 安全说明
//! - 生产环境中，客户端只通过代理地址访问，不直接暴露容器内部 IP
//! - user_id 到 container_ip 的映射在 Pingora 内部管理

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument, warn};
use utoipa::ToSchema;

use super::utils::I18nPath;
use super::utils::get_locale_from_headers;
use crate::{AppError, HttpResult, router::AppState, service::ComputerContainerManager};

/// VNC 桌面路径参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct DesktopPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
}

/// VNC 桌面访问响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopAccessResponse {
    /// 操作是否成功
    pub success: bool,

    /// Pingora 代理的 VNC 访问 URL（推荐使用）
    #[schema(example = "/computer/vnc/user_123/proj_456/vnc.html")]
    pub proxy_vnc_url: String,

    /// Pingora 代理的 WebSocket 连接 URL（推荐使用）
    #[schema(example = "/computer/vnc/user_123/proj_456/websockify")]
    pub proxy_websocket_url: String,

    /// 直接访问的 noVNC URL（仅开发/测试使用）
    #[schema(example = "http://172.17.0.5:6080/vnc.html")]
    pub direct_vnc_url: String,

    /// 直接访问的 WebSocket URL（仅开发/测试使用）
    #[schema(example = "ws://172.17.0.5:6080/websockify")]
    pub direct_websocket_url: String,

    /// 容器 ID
    pub container_id: String,

    /// 容器 IP 地址（内部 IP，不应直接暴露给外部客户端）
    pub container_ip: String,

    /// 用户 ID
    pub user_id: String,

    /// 项目 ID
    pub project_id: String,

    /// 访问提示
    #[schema(example = "请使用 proxy_vnc_url 或 proxy_websocket_url 访问 VNC 桌面")]
    pub message: String,
}

/// 错误响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopErrorResponse {
    /// 错误代码
    pub error: String,
    /// 错误消息
    pub message: String,
    /// 用户 ID
    pub user_id: String,
    /// 项目 ID
    pub project_id: String,
}

/// noVNC 默认端口
const NOVNC_PORT: u16 = 6080;

/// 获取 VNC 桌面访问信息
///
/// 返回 VNC 桌面的访问 URL。
/// 推荐使用 Pingora 代理路径（proxy_vnc_url / proxy_websocket_url）访问，
/// 这样可以避免暴露容器内部 IP。
///
/// ## 访问方式
/// - **推荐**: 使用 `proxy_vnc_url` 通过 Pingora 代理访问
/// - **开发测试**: 可以使用 `direct_vnc_url` 直接访问（需要网络互通）
#[utoipa::path(
    get,
    path = "/computer/desktop/{user_id}/{project_id}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID")
    ),
    responses(
        (
            status = 200,
            description = "成功获取 VNC 访问信息",
            body = HttpResult<DesktopAccessResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "proxy_vnc_url": "/computer/vnc/user_123/proj_456/vnc.html",
                    "proxy_websocket_url": "/computer/vnc/user_123/proj_456/websockify",
                    "direct_vnc_url": "http://172.17.0.5:6080/vnc.html",
                    "direct_websocket_url": "ws://172.17.0.5:6080/websockify",
                    "container_id": "abc123def456",
                    "container_ip": "172.17.0.5",
                    "user_id": "user_123",
                    "project_id": "proj_456",
                    "message": "请使用 proxy_vnc_url 访问 VNC 桌面"
                },
                "error": null
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到用户容器",
            body = HttpResult<DesktopErrorResponse>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_desktop_vnc",
    summary = "获取 VNC 桌面访问信息",
    description = "返回 VNC 桌面的访问 URL，推荐使用 Pingora 代理路径访问"
)]
#[instrument(skip(_state), fields(user_id = %params.user_id, project_id = %params.project_id))]
pub async fn computer_desktop_vnc(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nPath(params): I18nPath<DesktopPathParams>,
) -> Result<HttpResult<DesktopAccessResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let user_id = params.user_id.clone();
    let project_id = params.project_id.clone();

    // 1. 验证参数
    if user_id.trim().is_empty() {
        error!("[DESKTOP_VNC] user_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &shared_types::get_i18n_message("error.user_id_required", locale),
        ));
    }

    if project_id.trim().is_empty() {
        error!("[DESKTOP_VNC] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &shared_types::get_i18n_message("error.project_id_required", locale),
        ));
    }

    info!(
        "🖥️ [DESKTOP_VNC] 获取 VNC 访问信息: user_id={}, project_id={}",
        user_id, project_id
    );

    // 2. 查找用户容器
    let container_info = ComputerContainerManager::get_container_info(&user_id).await?;

    let container_info = match container_info {
        Some(info) => info,
        None => {
            warn!("[DESKTOP_VNC] message container: user_id={}", user_id);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
                &shared_types::get_i18n_message("error.container_not_found", locale),
            ));
        }
    };

    info!(
        "📦 [DESKTOP_VNC] 找到容器: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 3. 构建 VNC 访问 URL
    let container_ip = &container_info.container_ip;

    // Pingora 代理路径（推荐使用）
    let proxy_vnc_url = format!("/computer/vnc/{}/{}/vnc.html", user_id, project_id);
    let proxy_websocket_url = format!("/computer/vnc/{}/{}/websockify", user_id, project_id);

    // 直接访问路径（仅开发测试使用）
    let direct_vnc_url = format!("http://{}:{}/vnc.html", container_ip, NOVNC_PORT);
    let direct_websocket_url = format!("ws://{}:{}/websockify", container_ip, NOVNC_PORT);

    // 4. 返回访问信息
    let response = DesktopAccessResponse {
        success: true,
        proxy_vnc_url: proxy_vnc_url.clone(),
        proxy_websocket_url: proxy_websocket_url.clone(),
        direct_vnc_url,
        direct_websocket_url,
        container_id: container_info.container_id.clone(),
        container_ip: container_ip.clone(),
        user_id: user_id.clone(),
        project_id: project_id.clone(),
        message: "请使用 proxy_vnc_url 或 proxy_websocket_url 通过 Pingora 代理访问 VNC 桌面"
            .to_string(),
    };

    info!(
        "✅ [DESKTOP_VNC] VNC 访问信息已生成: user_id={}, proxy_vnc_url={}",
        user_id, proxy_vnc_url
    );

    Ok(HttpResult::success(response))
}

/// VNC 桌面代理路径参数（用于 Pingora 代理）
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct VncProxyPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
    /// 剩余路径（可选）
    #[schema(example = "vnc.html", nullable = true)]
    pub path: Option<String>,
}

/// VNC 桌面代理路径（通过 Pingora 代理）
///
/// 这是一个占位实现，用于生成 OpenAPI 文档。
/// 实际的 VNC 代理请求会通过 Pingora 透明代理到容器的 noVNC 服务。
///
/// ## 路径说明
/// - `GET /computer/vnc/{user_id}/{project_id}/vnc.html` - VNC 桌面页面
/// - `GET /computer/vnc/{user_id}/{project_id}/websockify` - VNC WebSocket 连接
/// - `GET /computer/vnc/{user_id}/{project_id}/{*path}` - 其他 noVNC 资源
///
/// ## 实现说明
/// 完整的 WebSocket 代理需要：
/// 1. HTTP Upgrade 处理
/// 2. 双向 WebSocket 帧转发
/// 3. 连接生命周期管理
///
/// 当前实现使用 Pingora 透明代理，客户端请求会直接代理到容器内部服务。
#[utoipa::path(
    get,
    path = "/computer/vnc/{user_id}/{project_id}/{*path}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID"),
        ("path" = Option<String>, Path, description = "剩余路径，如 vnc.html, websockify 等")
    ),
    responses(
        (
            status = 200,
            description = "成功访问 VNC 资源",
            body = String,
            example = "<!DOCTYPE html>\\n<html>\\n<head><title>noVNC</title></head>\\n<body>noVNC Client</body>\\n</html>"
        ),
        (
            status = 101,
            description = "WebSocket 升级响应",
            body = String
        ),
        (
            status = 404,
            description = "找不到用户容器或资源不存在",
            body = HttpResult<DesktopErrorResponse>,
            example = json!({
                "success": false,
                "data": null,
                "code": "PROXY_REDIRECT",
                "message": "请使用 Pingora 代理路径访问 VNC 桌面，路径: /computer/vnc/user_123/proj_456/vnc.html",
                "tid": null
            })
        )
    ),
    tag = "computer",
    operation_id = "computer_vnc_proxy",
    summary = "VNC 桌面代理",
    description = r#"
通过 Pingora 代理访问容器的 VNC 桌面服务。

## 访问方式

### VNC 桌面页面
```
GET /computer/vnc/{user_id}/{project_id}/vnc.html
```

### WebSocket 连接
```
GET /computer/vnc/{user_id}/{project_id}/websockify
```

### 其他资源
```
GET /computer/vnc/{user_id}/{project_id}/{*path}
```

## 工作原理

1. 客户端请求到达 RCoder 服务
2. Axum 路由器匹配到 VNC 代理路径
3. 请求转发给 Pingora 代理服务
4. Pingora 根据 user_id 查找容器 IP
5. Pingora 透明代理请求到容器的 noVNC 服务（端口 6080）
6. 响应返回给客户端

## 使用示例

```javascript
// 访问 VNC 桌面页面
window.open('/computer/vnc/user_123/proj_456/vnc.html', '_blank');

// 或在 iframe 中嵌入
<iframe src="/computer/vnc/user_123/proj_456/vnc.html" width="100%" height="600"></iframe>
```
"#
)]
#[allow(dead_code)]
pub async fn computer_desktop_proxy(
    State(_state): State<Arc<AppState>>,
    I18nPath((user_id, project_id, path)): I18nPath<(String, String, Option<String>)>,
) -> impl IntoResponse {
    // 占位实现：实际代理由 Pingora 处理
    // 这里返回 501 是为了表明这个端点应该由 Pingora 代理

    let error_response = DesktopErrorResponse {
        error: "PROXY_REDIRECT".to_string(),
        message: format!(
            "请使用 Pingora 代理路径访问 VNC 桌面，路径: /computer/vnc/{}/{}/{}",
            user_id,
            project_id,
            path.as_deref().unwrap_or("vnc.html")
        ),
        user_id,
        project_id,
    };

    (StatusCode::NOT_IMPLEMENTED, Json(error_response))
}

/// 音频代理路径参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct AudioProxyPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
    /// 剩余路径
    ///
    /// ## 路径说明
    /// - 空字符串或 `index.html`: 音频播放器页面（HTTP 6090）
    /// - `ws`: 音频流 WebSocket（端口 6089）
    /// - `ws/{token}`: 带认证的音频流 WebSocket
    #[schema(example = "index.html", nullable = true)]
    pub path: Option<String>,
}

/// 音频流代理（通过 Pingora 代理）
///
/// 这是一个占位实现，用于生成 OpenAPI 文档。
/// 实际的音频代理请求会通过 Pingora 透明代理到容器的音频服务。
///
/// ## 路径说明
/// - `GET /computer/audio/{user_id}/{project_id}/` - 音频播放器页面
/// - `GET /computer/audio/{user_id}/{project_id}/index.html` - 音频播放器页面
/// - `GET /computer/audio/{user_id}/{project_id}/ws` - 音频流 WebSocket（Opus 编码）
///
/// ## 端口说明
/// - **HTTP 6090**: 音频播放器页面和静态文件服务
/// - **WebSocket 6089**: Opus 音频流（48kHz 双声道）
///
/// ## 工作原理
/// 1. 客户端请求到达 RCoder 服务
/// 2. Pingora 根据 user_id 查找容器 IP
/// 3. 路径判断：`ws` 或 `ws/*` → WebSocket 端口 6089，其他 → HTTP 端口 6090
/// 4. Pingora 透明代理请求到容器的音频服务
/// 5. 音频流使用 WebSocket 传输 Opus 编码的音频数据
#[utoipa::path(
    get,
    path = "/computer/audio/{user_id}/{project_id}/{*path}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID"),
        ("path" = Option<String>, Path, description = "剩余路径")
    ),
    responses(
        (
            status = 200,
            description = "成功访问音频播放器页面",
            body = String,
            content_type = "text/html",
            example = "<!DOCTYPE html>\\n<html>\\n<head><title>Audio Player</title></head>\\n<body>...</body>\\n</html>"
        ),
        (
            status = 101,
            description = "WebSocket 升级响应（音频流）",
            body = String
        ),
        (
            status = 404,
            description = "找不到用户容器或资源不存在",
            body = HttpResult<DesktopErrorResponse>
        ),
        (
            status = 503,
            description = "代理服务未启用",
            body = HttpResult<DesktopErrorResponse>
        )
    ),
    tag = "computer",
    operation_id = "computer_audio_proxy",
    summary = "音频流代理",
    description = r#"
通过 Pingora 代理访问容器的音频流服务。

## 访问方式

### 音频播放器页面
```
GET /computer/audio/{user_id}/{project_id}/
GET /computer/audio/{user_id}/{project_id}/index.html
```

### 音频流 WebSocket
```
WebSocket /computer/audio/{user_id}/{project_id}/ws
```

## 工作原理

1. 客户端请求到达 RCoder 服务
2. Axum 路由器匹配到音频代理路径
3. 请求转发给 Pingora 代理服务
4. Pingora 根据 user_id 查找容器 IP
5. **路径判断**:
   - `path == "ws"` 或 `path.starts_with("ws/")` → WebSocket 端口 6089
   - 其他（包括空路径）→ HTTP 端口 6090
6. Pingora 透明代理请求到容器的音频服务
7. 响应返回给客户端

## 音频编码格式

- **编码**: Opus
- **采样率**: 48kHz
- **声道**: 双声道（Stereo）
- **传输**: WebSocket 二进制帧

## 使用示例

```javascript
// 访问音频播放器页面
window.open('/computer/audio/user_123/proj_456/', '_blank');

// 连接音频流 WebSocket
const ws = new WebSocket('ws://localhost:8088/computer/audio/user_123/proj_456/ws');
ws.binaryType = 'arraybuffer';
ws.onmessage = (event) => {
    const opusData = new Uint8Array(event.data);
    // 解码 Opus → PCM → 播放
};
```
"#
)]
#[allow(dead_code)]
pub async fn computer_audio_proxy(
    State(_state): State<Arc<AppState>>,
    I18nPath((user_id, project_id, path)): I18nPath<(String, String, Option<String>)>,
) -> impl IntoResponse {
    let error_response = DesktopErrorResponse {
        error: "PROXY_REDIRECT".to_string(),
        message: format!(
            "请使用 Pingora 代理路径访问音频服务，路径: /computer/audio/{}/{}/{}",
            user_id,
            project_id,
            path.as_deref().unwrap_or("")
        ),
        user_id,
        project_id,
    };

    (StatusCode::NOT_IMPLEMENTED, Json(error_response))
}

/// IME 输入法代理路径参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct ImeProxyPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
    /// 剩余路径
    ///
    /// ## 路径说明
    /// - `connect`: IME WebSocket 连接端点（端口 6091）
    /// - 其他值会被转发到 IME 服务
    #[schema(example = "connect", nullable = true)]
    pub path: Option<String>,
}

/// IME 输入法代理（通过 Pingora 代理）
///
/// 这是一个占位实现，用于生成 OpenAPI 文档。
/// 实际的 IME 代理请求会通过 Pingora 透明代理到容器的 IME 服务。
///
/// ## 路径说明
/// - `WebSocket /computer/ime/{user_id}/{project_id}/connect` - IME WebSocket 连接
///
/// ## 端口说明
/// - **WebSocket 6091**: IME 输入法透传服务
///
/// ## 工作原理
/// 客户端本地输入法（浏览器 IME）通过 WebSocket 发送文本到 Pingora，
/// Pingora 代理到容器 IME 服务，容器使用 xdotool 将文本输入到远程桌面。
#[utoipa::path(
    get,
    path = "/computer/ime/{user_id}/{project_id}/{*path}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID"),
        ("path" = Option<String>, Path, description = "剩余路径")
    ),
    responses(
        (
            status = 101,
            description = "WebSocket 升级响应（IME 连接）",
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
    operation_id = "computer_ime_proxy",
    summary = "IME 输入法代理",
    description = r#"
通过 Pingora 代理访问容器的 IME 输入法透传服务。

## 访问方式

### IME WebSocket 连接
```
WebSocket /computer/ime/{user_id}/{project_id}/connect
```

## 工作原理

1. **客户端**: 浏览器本地 IME 输入中文
2. **WebSocket 发送**: 将文本通过 WebSocket 发送到 Pingora
3. **Pingora 代理**: 根据 user_id 查找容器 IP，代理到端口 6091
4. **容器 IME 服务**: 接收文本，使用 xdotool 输入到远程桌面
5. **远程桌面**: 显示输入的文本

## 消息格式

### 客户端 → 容器
```json
{
  "type": "text",
  "text": "你好，世界",
  "method": "xdotool"
}
```

### 容器 → 客户端
```json
{
  "status": "success",
  "message": "文本已输入"
}
```

## 使用示例

```javascript
// 连接 IME WebSocket
const imeWs = new WebSocket('ws://localhost:8088/computer/ime/user_123/proj_456/connect');

// 发送文本
imeWs.send(JSON.stringify({
  type: 'text',
  text: '测试中文输入',
  method: 'xdotool'
}));

// 接收响应
imeWs.onmessage = (event) => {
  const response = JSON.parse(event.data);
  console.log('IME 响应:', response);
};
```

## 安全说明

- 文本长度限制：1000 字符
- 危险控制字符过滤（NULL, ESC）
- 使用 `--` 参数分隔符防止命令注入
"#
)]
#[allow(dead_code)]
pub async fn computer_ime_proxy(
    State(_state): State<Arc<AppState>>,
    I18nPath((user_id, project_id, path)): I18nPath<(String, String, Option<String>)>,
) -> impl IntoResponse {
    let error_response = DesktopErrorResponse {
        error: "PROXY_REDIRECT".to_string(),
        message: format!(
            "请使用 Pingora 代理路径访问 IME 服务，路径: /computer/ime/{}/{}/{}",
            user_id,
            project_id,
            path.as_deref().unwrap_or("connect")
        ),
        user_id,
        project_id,
    };

    (StatusCode::NOT_IMPLEMENTED, Json(error_response))
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 检查 VNC 服务是否可用
///
/// 尝试连接到容器的 noVNC 端口，验证服务是否运行
#[allow(dead_code)]
async fn check_vnc_available(container_ip: &str) -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};

    let addr = format!("{}:{}", container_ip, NOVNC_PORT);

    match timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            info!("[DESKTOP_VNC] VNC message : {}", addr);
            true
        }
        Ok(Err(e)) => {
            warn!("[DESKTOP_VNC] VNC connectionfailed: {} - {}", addr, e);
            false
        }
        Err(_) => {
            warn!("[DESKTOP_VNC] VNC connectiontimeout: {}", addr);
            false
        }
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vnc_url_format() {
        let ip = "172.17.0.5";
        let user_id = "user_123";
        let project_id = "proj_456";

        // Pingora 代理路径
        let proxy_vnc_url = format!("/computer/vnc/{}/{}/vnc.html", user_id, project_id);
        let proxy_ws_url = format!("/computer/vnc/{}/{}/websockify", user_id, project_id);

        // 直接访问路径
        let direct_vnc_url = format!("http://{}:{}/vnc.html", ip, NOVNC_PORT);
        let direct_ws_url = format!("ws://{}:{}/websockify", ip, NOVNC_PORT);

        assert_eq!(proxy_vnc_url, "/computer/vnc/user_123/proj_456/vnc.html");
        assert_eq!(proxy_ws_url, "/computer/vnc/user_123/proj_456/websockify");
        assert_eq!(direct_vnc_url, "http://172.17.0.5:6080/vnc.html");
        assert_eq!(direct_ws_url, "ws://172.17.0.5:6080/websockify");
    }

    #[test]
    fn test_desktop_access_response_serialization() {
        let response = DesktopAccessResponse {
            success: true,
            proxy_vnc_url: "/computer/vnc/user_123/proj_456/vnc.html".to_string(),
            proxy_websocket_url: "/computer/vnc/user_123/proj_456/websockify".to_string(),
            direct_vnc_url: "http://172.17.0.5:6080/vnc.html".to_string(),
            direct_websocket_url: "ws://172.17.0.5:6080/websockify".to_string(),
            container_id: "abc123".to_string(),
            container_ip: "172.17.0.5".to_string(),
            user_id: "user_123".to_string(),
            project_id: "proj_456".to_string(),
            message: "Test message".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("proxy_vnc_url"));
        assert!(json.contains("proxy_websocket_url"));
        assert!(json.contains("direct_vnc_url"));
        assert!(json.contains("user_123"));
    }
}

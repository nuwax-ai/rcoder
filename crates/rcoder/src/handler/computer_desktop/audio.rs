//! computer 桌面 audio 代理（从 computer_desktop_handler.rs 拆出）。

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
            status = 501,
            description = "未实现：本端点由 Pingora 代理，直接调用 rcoder 返回此占位响应",
            body = DesktopErrorResponse
        ),
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

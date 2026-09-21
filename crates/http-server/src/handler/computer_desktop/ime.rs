//! computer 桌面 ime 代理（从 computer_desktop_handler.rs 拆出）。

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
            status = 501,
            description = "未实现：本端点由 Pingora 代理，直接调用 rcoder 返回此占位响应",
            body = DesktopErrorResponse
        ),
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

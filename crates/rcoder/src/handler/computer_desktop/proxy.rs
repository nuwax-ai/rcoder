//! computer 桌面总入口 proxy + 响应 DTO（从 computer_desktop_handler.rs 拆出）。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use super::super::utils::I18nPath;
use crate::HttpResult;
use crate::router::AppState;

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
            status = 501,
            description = "未实现：本端点由 Pingora 代理，直接调用 rcoder 返回此占位响应",
            body = DesktopErrorResponse
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::NOVNC_PORT;

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

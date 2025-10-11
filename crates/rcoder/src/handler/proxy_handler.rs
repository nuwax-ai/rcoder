use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
    body::Body,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::router::AppState;
use pingora_proxy::{ProxyServer, ProxyConfig};

/// 代理查询参数
#[derive(Debug, Deserialize)]
pub struct ProxyQuery {
    /// 目标端口
    pub port: u16,
    /// 目标路径
    pub path: Option<String>,
}


/// 代理错误响应
#[derive(Debug, Serialize)]
pub struct ProxyErrorResponse {
    error: String,
    message: String,
}

/// 处理代理请求（查询参数方式）
///
/// 路径格式: /proxy?port=8766&path=/api/users
pub async fn handle_proxy_request(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProxyQuery>,
    request: Request<Body>,
) -> impl IntoResponse {
    handle_proxy_to_port_with_path(state, query.port, query.path, request).await
}

/// 处理带路径的代理请求
async fn handle_proxy_to_port_with_path(
    state: Arc<AppState>,
    target_port: u16,
    target_path: Option<String>,
    request: Request<Body>,
) -> Response {
    // 获取代理服务
    let proxy_service = match &state.proxy_server {
        Some(server) => server.service(),
        None => {
            let error_response = ProxyErrorResponse {
                error: "PROXY_NOT_AVAILABLE".to_string(),
                message: "代理服务未初始化".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    // 确保后端存在
    proxy_service.add_backend(target_port, "127.0.0.1".to_string()).await;

    // 修改请求以包含目标端口和路径信息
    let modified_request = modify_request_for_proxy_with_path(request, target_port, target_path).await;

    // 发送代理请求
    match proxy_service.proxy_request(modified_request).await {
        Ok(proxy_response) => {
            proxy_response
        }
        Err(err) => {
            tracing::error!("代理请求失败: {}", err);
            let error_response = ProxyErrorResponse {
                error: "PROXY_ERROR".to_string(),
                message: format!("代理请求失败: {}", err),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_response)).into_response()
        }
    }
}

/// 修改请求以包含目标端口和路径信息
async fn modify_request_for_proxy_with_path(request: Request<Body>, target_port: u16, target_path: Option<String>) -> Request<Body> {
    // 获取原始请求信息
    let original_uri = request.uri().clone();
    let original_method = request.method().clone();

    // 构建新的URI
    let host = original_uri.host().unwrap_or("localhost");
    let original_query = original_uri.query();

    // 使用目标路径，如果没有提供则使用原始路径
    let final_path = target_path.unwrap_or_else(|| original_uri.path().to_string());

    // 添加port查询参数，保留其他查询参数
    let final_query = if let Some(query) = original_query {
        // 解析现有查询参数并添加port参数
        let mut param_pairs: Vec<String> = Vec::new();

        // 先添加现有的查询参数（除了port，避免重复）
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key != "port" {
                    param_pairs.push(format!("{}={}", key, value));
                }
            }
        }

        // 添加port参数
        param_pairs.push(format!("port={}", target_port));

        // 构建查询字符串
        param_pairs.join("&")
    } else {
        format!("port={}", target_port)
    };

    // 构建完整的URL
    let target_url = if !final_query.is_empty() {
        format!("http://{}:{}{}?{}", host, target_port, final_path, final_query)
    } else {
        format!("http://{}:{}{}", host, target_port, final_path)
    };

    let new_uri = Uri::from_str(&target_url)
        .expect("Failed to build target URI");

    // 构建新的请求
    let mut builder = Request::builder()
        .method(original_method)
        .uri(new_uri);

    // 复制请求头，过滤掉一些不应该转发的头
    for (name, value) in request.headers() {
        let name_str = name.as_str();
        if !should_skip_header(name_str) {
            builder = builder.header(name, value);
        }
    }

    // 复制请求体
    let body_bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => axum::body::Bytes::new(),
    };

    builder
        .body(Body::from(body_bytes))
        .expect("Failed to build request")
}

/// 构建目标URI
fn build_target_uri(
    original_uri: &Uri,
    target_port: u16,
    method: &Method,
    original_path: &str,
) -> Result<Uri, Box<dyn std::error::Error + Send + Sync>> {
    // 获取主机信息
    let host = original_uri
        .host()
        .unwrap_or("localhost");

    // 处理路径
    let path = if original_path.starts_with("/proxy/") {
        // 移除 /proxy/{port}/ 前缀
        let parts: Vec<&str> = original_path.split('/').collect();
        if parts.len() >= 3 {
            parts[3..].join("/")
        } else {
            "/".to_string()
        }
    } else {
        original_path.to_string()
    };

    // 获取查询参数
    let query = original_uri.query();

    // 构建目标URL
    let target_url = if let Some(query) = query {
        format!("http://{}:{}{}?{}", host, target_port, path, query)
    } else {
        format!("http://{}:{}{}", host, target_port, path)
    };

    tracing::debug!("代理请求: {} {} -> {}", method, original_uri, target_url);

    Ok(Uri::from_str(&target_url)?)
}

/// 构建代理请求
async fn build_proxy_request(
    target_uri: &Uri,
    original_request: Request<Body>,
    headers: &HeaderMap,
) -> Result<Request<Body>, Box<dyn std::error::Error + Send + Sync>> {

    let mut builder = Request::builder()
        .method(original_request.method())
        .uri(target_uri.clone());

    // 复制请求头，过滤掉一些不应该转发的头
    for (name, value) in headers {
        let name_str = name.as_str();

        // 跳过一些不应该转发的头
        if !should_skip_header(name_str) {
            builder = builder.header(name, value);
        }
    }

    // 复制请求体（简化处理）
    let body_bytes = match axum::body::to_bytes(original_request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return Err(format!("读取请求体失败: {}", err).into());
        }
    };

    builder
        .body(Body::from(body_bytes))
        .map_err(|e| format!("构建代理请求失败: {}", e).into())
}

/// 判断是否应该跳过某个请求头
fn should_skip_header(header_name: &str) -> bool {
    matches!(
        header_name.to_lowercase().as_str(),
        "host" | "connection" | "keep-alive" | "te" | "trailer" |
        "transfer-encoding" | "upgrade" | "proxy-authorization" |
        "proxy-connection" | "proxy-authenticate"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{HeaderValue, Method};

    #[test]
    fn test_should_skip_header() {
        assert!(should_skip_header("host"));
        assert!(should_skip_header("connection"));
        assert!(should_skip_header("keep-alive"));
        assert!(!should_skip_header("content-type"));
        assert!(!should_skip_header("authorization"));
    }

    #[test]
    fn test_build_target_uri() {
        let uri = Uri::from_static("http://localhost:8080/proxy/8766/api/users?active=true");
        let method = Method::GET;

        let result = build_target_uri(&uri, 8766, &method, "/proxy/8766/api/users");
        assert!(result.is_ok());

        let target_uri = result.unwrap();
        assert_eq!(target_uri.host(), Some("localhost"));
        assert_eq!(target_uri.port(), Some(8766));
        assert_eq!(target_uri.path(), "/api/users");
        assert_eq!(target_uri.query(), Some("active=true"));
    }
}
//! 路由配置模块（目录化拆分）
//!
//! 集中管理 Pingora 代理服务的路由注册：基础路由（端口代理 / 健康检查 /
//! API 密钥代理）在本文件，computer 域（VNC / 音频 / IME / ttyd）与
//! userApp 域（流量族 / 工具族 / dbx）分属 `computer` / `userapp`。
//!
//! ## 路由语法
//!
//! - `{param}`: 命名参数，匹配单个路径段（例如 `{user_id}` 匹配 "user_123"）
//! - `{*path}`: 通配符参数，匹配剩余所有路径（必须在最后，至少 1 字符——
//!   无尾随 path 的根形态须单独注册兜底路由，本模块各族均成对注册）
//!
//! ## 路由优先级
//!
//! matchit 使用 radix tree 结构：静态路径段优先于参数，命名参数优先于通配符。
//! 路由冲突会在插入时报错（通常在开发阶段就能发现）。

mod computer;
mod userapp;

#[cfg(test)]
mod tests;

use matchit::Router;

// RouteType 拆至 route_type.rs（枚举+文档）；re-export 保持 crate::router::RouteType 引用稳定
pub use crate::route_type::RouteType;

/// 插入单条路由；pattern 冲突/非法统一报 `RouteConfig` 错误（含 pattern 便于定位）。
fn insert_route(
    router: &mut Router<RouteType>,
    pattern: &str,
    route: RouteType,
) -> Result<(), crate::ProxyError> {
    router.insert(pattern, route).map_err(|e| {
        tracing::error!("[ROUTER] route config failed: {pattern}: {e}");
        crate::ProxyError::RouteConfig(format!("route {pattern} configuration error: {e}"))
    })
}

/// 创建路由表
///
/// 初始化并配置所有支持的路由规则（注册顺序与拆分前一致）。
pub fn create_router() -> Result<Router<RouteType>, crate::ProxyError> {
    let mut router = Router::new();

    computer::insert_vnc_routes(&mut router)?;
    insert_port_proxy_routes(&mut router)?;
    userapp::insert_userapp_routes(&mut router)?;
    insert_health_and_api_routes(&mut router)?;
    computer::insert_terminal_routes(&mut router)?;

    Ok(router)
}

/// 端口反向代理：`/proxy/{port}/{*path}` + 根路径兜底。
///
/// 兜底：访问 app 根路径 `/proxy/{port}`（无尾随 path，如
/// http://host:port/proxy/80）。matchit 的 `{*path}` 要求至少 1 个字符，
/// `/proxy/{port}/{*path}` 无法匹配 2 段路径，单独注册让根路径访问能命中
/// PortProxy（handler 已把空 path 归一为 "/"，转发到 backend 根），否则
/// `/proxy/80` → route not found → 404，app 根路径完全不可达。
fn insert_port_proxy_routes(router: &mut Router<RouteType>) -> Result<(), crate::ProxyError> {
    insert_route(router, "/proxy/{port}/{*path}", RouteType::PortProxy)?;
    insert_route(router, "/proxy/{port}", RouteType::PortProxy)
}

/// 健康检查 `/health` + 🔒 API 密钥代理 `/api/{service_name}/{*path}`。
///
/// API 密钥代理拦截 AI API 请求，注入真实密钥后转发：
/// 1. 客户端使用占位密钥 (sk-placeholder) 发送请求到本地代理
/// 2. Pingora 从 ApiKeyManager 读取真实密钥
/// 3. 移除占位密钥，注入真实密钥到请求头
/// 4. 重写 URI 到真实 API 端点
///
/// Fallback `/api/{service_name}`（无额外路径段）用于 Claude Code 启动时
/// 对 base URL 的 HEAD 连通性检查。
fn insert_health_and_api_routes(router: &mut Router<RouteType>) -> Result<(), crate::ProxyError> {
    insert_route(router, "/health", RouteType::HealthCheck)?;
    insert_route(router, "/api/{service_name}/{*path}", RouteType::ApiProxy)?;
    insert_route(router, "/api/{service_name}", RouteType::ApiProxy)
}

//! computer 域路由注册：VNC / 音频 / IME / ttyd / Web ttyd（从 router.rs 拆出；
//! pattern 原样、注册顺序不变）。

use matchit::Router;

use super::insert_route;
use crate::route_type::RouteType;

/// VNC WebSocket 代理：`/computer/vnc/{user_id}/{project_id}/{*path}`。
///
/// 将 WebSocket 请求代理到用户容器的 noVNC 服务（端口 6080）：
/// - /computer/vnc/user_123/proj_456/vnc.html -> 容器IP:6080/vnc.html
/// - /computer/vnc/user_123/proj_456/websockify -> 容器IP:6080/websockify (WebSocket)
pub(super) fn insert_vnc_routes(router: &mut Router<RouteType>) -> Result<(), crate::ProxyError> {
    insert_route(
        router,
        "/computer/vnc/{user_id}/{project_id}/{*path}",
        RouteType::VncProxy,
    )
}

/// computer 终端/外设族：音频 / IME / ttyd（单端口双协议）+ Web ttyd。
///
/// ttyd 用 libwebsockets 同端口监听 HTTP 和 WebSocket，pingora 默认透传
/// `Connection: Upgrade` 头，service 层无需特殊处理。Web ttyd 与 agent-runner
/// 的 ttyd 代理不同，代理到本地 127.0.0.1:7681 并通过 `--cwd` 设置工作目录。
pub(super) fn insert_terminal_routes(
    router: &mut Router<RouteType>,
) -> Result<(), crate::ProxyError> {
    // 音频流：HTTP 静态文件（6090）与 WebSocket 音频流（6089，path 以 ws 开头）
    insert_route(
        router,
        "/computer/audio/{user_id}/{project_id}/{*path}",
        RouteType::AudioProxy,
    )?;

    // IME 输入法（WebSocket 6091）
    insert_route(
        router,
        "/computer/ime/{user_id}/{project_id}/{*path}",
        RouteType::ImeProxy,
    )?;

    // computer ttyd（7681）+ 无尾随 path 兜底
    insert_route(
        router,
        "/computer/ttyd/{user_id}/{project_id}/{*path}",
        RouteType::TtydProxy,
    )?;
    insert_route(
        router,
        "/computer/ttyd/{user_id}/{project_id}",
        RouteType::TtydProxy,
    )?;

    // Web ttyd（127.0.0.1:7681，--cwd=/app/project_workspace/{project_id}）+ 兜底
    insert_route(
        router,
        "/web/ttyd/{user_id}/{project_id}/{*path}",
        RouteType::WebTtydProxy,
    )?;
    insert_route(
        router,
        "/web/ttyd/{user_id}/{project_id}",
        RouteType::WebTtydProxy,
    )
}

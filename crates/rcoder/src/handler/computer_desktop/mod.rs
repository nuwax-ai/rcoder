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

mod audio;
mod ime;
mod proxy;
mod ttyd;
mod vnc;

pub use audio::*;
pub use ime::*;
pub use proxy::*;
pub use ttyd::*;
pub use vnc::*;

// 各桌面协议（vnc/audio/ime/ttyd）handler 与各自 PathParams 同档；桌面总入口
// proxy 的三个响应 DTO 在 proxy.rs。

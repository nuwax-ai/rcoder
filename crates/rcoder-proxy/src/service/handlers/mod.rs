//! 请求处理函数模块
//!
//! 包含各种代理类型的请求处理和上游选择函数。

pub mod api_proxy;
pub mod app_proxy;
pub mod audio;
pub mod dbx;
pub mod dev_app_proxy;
pub mod dev_terminal;
pub mod ime;
pub mod port_proxy;
pub mod preview_forward;
pub mod ttyd;
pub mod ttyd_params;
pub mod vnc;

#[cfg(test)]
mod dev_terminal_tests;
#[cfg(test)]
mod preview_forward_tests;
#[cfg(test)]
mod ttyd_tests;

use std::time::Duration;

use pingora_core::upstreams::peer::HttpPeer;

/// 流式代理 peer 的统一超时配置（ttyd/VNC/audio/IME/dbx/app 等长连接场景）：
/// 连接建立 10s、读写不限时（双向持续流）、含 TLS 握手的总建连 15s、
/// 空闲超时由调用方显式传入（当前各流式入口均为 1 小时）。
pub(super) fn streaming_peer_options(peer: &mut HttpPeer, idle_timeout: Duration) {
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(idle_timeout);
}

//! ttyd WebSocket 终端中间层
//!
//! agent_runner 用 tokio-tungstenite 在浏览器和本地 ttyd 之间做 WS 中间控制层：
//! - 对外：监听 7681，接浏览器 WS（子协议 `tty`，实现 ttyd 二进制帧透传）
//! - 对内：用 `connect_async` 连本地 ttyd（`ws://127.0.0.1:17681/ws`），
//!   ttyd 退到内部端口（17681），仍提供真实 PTY/终端
//!
//! cd 逻辑由本模块代码每次连接（含重连）控制：从 Pingora 注入的
//! `X-Ttyd-Project-Id` header 拿 project_id，连接 ttyd 时注入 `arg=--cwd&arg={项目目录}`，
//! 彻底摆脱「Pingora `upstream_request_filter` 对 WS 只首次触发」的结构性缺陷。
//!
//! 后续可在中间层增量扩展：WS 鉴权、session 持久化、命令审计等。

pub mod cwd;
pub mod protocol;
pub mod proxy;
pub mod server;

pub use server::start_ws_terminal;

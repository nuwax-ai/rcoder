//! Pingora 代理 API 处理函数（目录化：按 status/port/app/legacy 域分组，
//! 经 mod.rs re-export——`handler::proxy_status` 等 fn 路径零改动）。
//!
//! 路由由 binary 端 router 注册（`--all-targets` 口径下 bin 也算调用点，无需抑制 dead_code）。

mod app;
mod legacy;
mod port;
mod status;

pub use app::*;
pub use legacy::*;
pub use port::*;
pub use status::*;

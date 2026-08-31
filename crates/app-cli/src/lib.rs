//! `app-cli`：Userapp 容器运行时编排器。
//!
//! 装在 app-runtime 镜像，替代 workspace `start.sh`：自动发现子项目 → 编排（启服务 + pingap）
//! → 日志（轮转 + API + SSE）→ 管理 API。
//!
//! 分层：
//! - [`config`]：CLI 参数
//! - [`manifest`]：自动发现子项目 → ServiceSpec
//! - [`supervisor`]：编排核心（wait PG → migrate → start → pingap → supervise）
//! - [`api`]：管理 HTTP 端点（/health /reload /logs /logs/stream SSE）
//! - [`log`]：日志系统（轮转写入 + 历史读取 + 实时流）
//! - [`proxy`]：pingap 配置生成

pub mod api;
pub mod config;
pub mod deploy;
pub mod devtool;
pub mod idle;
pub mod log;
pub mod manifest;
pub mod proxy;
pub mod run_service;
pub mod runtime_status;
pub mod server;
pub mod supervisor;
pub mod supervisord_host;
pub mod svc_spec;
pub mod xmlrpc;

pub use config::CliArgs;

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

// 测试构建豁免 unsafe_code deny：edition 2024 的 env 变异（set_var/
// remove_var）标记为 unsafe，测试模块需要变异 APP_CLI_STATE_ROOT 等环境。
// 仅 cfg(test)；生产代码禁 unsafe 不变。
#![cfg_attr(test, allow(unsafe_code))]

pub mod api;
pub mod build;
pub mod business_readiness;
pub mod config;
pub mod deploy;
pub mod devtool;
pub mod idle;
pub mod log;
pub mod manifest;
mod migration_journal;
pub mod orchestration_events;
pub mod owner_dispatch;
pub mod platform;
pub mod proxy;
pub mod readiness_query;
pub mod run_service;
pub mod runtime_kernel;
pub mod runtime_status;
pub mod server;
pub mod static_hosting;
pub mod supervisor;
pub mod supervisord_host;
pub mod svc_spec;
pub mod win_cmd;
pub mod workspace_index;
pub mod xmlrpc;

pub use config::{CliArgs, RuntimeArgs};

//! `app-cli`：Userapp 容器运行时编排器。
//!
//! 装在 app-runtime 镜像，替代 workspace `start.sh`：自动发现子项目 → 编排（启服务 + pingap）
//! → 日志（轮转 + API + SSE）→ 管理 API。
//!
//! 分层（域目录 + 根级兼容别名）：
//! - [`control`]：控制面（CLI 参数、双 CLI 监督、owner 分派、就绪查询）
//! - [`build_deploy`]：构建与部署（manifest 解析、构建、两阶段部署、gen-lock）
//! - [`orchestration`]：编排内核（serve 状态机、操作受理、就绪判定、编排事件）
//! - [`services`]：服务承载（supervisor、supervisord/XML-RPC、静态托管、工作区索引）
//! - [`api`]：管理 HTTP 端点（/health /reload /logs /logs/stream SSE）
//! - [`log`]：日志系统（轮转写入 + 历史读取 + 实时流）
//! - [`proxy`]：pingap 配置生成
//! - [`platform`]：跨平台锁、进程树与 Windows .cmd shim

// 测试构建豁免 unsafe_code deny：edition 2024 的 env 变异（set_var/
// remove_var）标记为 unsafe，测试模块需要变异 APP_CLI_STATE_ROOT 等环境。
// 仅 cfg(test)；生产代码禁 unsafe 不变。
#![cfg_attr(test, allow(unsafe_code))]

pub mod api;
pub mod build_deploy;
pub mod control;
pub mod log;
pub mod orchestration;
pub mod platform;
pub mod proxy;
pub mod services;

// 根级兼容别名：域目录归位前的历史路径（crate::server 等）保持可用
// （file-server 大拆分同款范式），跨模块引用零改动；新代码优先用域路径。
pub use build_deploy::{build, deploy, devtool, manifest};
pub use control::{config, owner_dispatch, readiness_query, supervision};
pub use orchestration::{
    business_readiness, idle, orchestration_events, runtime_kernel, runtime_status, server,
};
pub use platform::win_cmd;
pub use services::{
    run_service, static_hosting, supervisor, supervisord_host, svc_spec, workspace_index, xmlrpc,
};
// 原 lib.rs 即为私有的两个模块：别名保持 crate 内可见性不变。
pub(crate) use orchestration::{migration_journal, startup_probe};

pub use config::{CliArgs, RuntimeArgs};

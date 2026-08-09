//! Handler 工具模块
//!
//! 提供 handler 层共享的工具函数和常量。

mod agent_diagnostic;
mod agent_mgmt_forward;
mod grpc_addr;
mod i18n_extractors;
mod locale;
mod paths;

// root_cause_message 仅被 lib 的 grpc::session_stream_registry 使用(bin 没有 grpc 模块),
// bin 编译本 mod 时会判 unused —— 与本文件既有 #[allow(unused_imports)] 同一处理(lib/bin 双编译)。
#[allow(unused_imports)]
pub use agent_diagnostic::{
    build_connection_error, diagnose, root_cause_message, wait_agent_ready,
};
// DiagCtx 定义在共享 crate container-runtime-api,这里 re-export 供 rcoder 内部统一引用
// (规避 lib/bin 双实例类型分裂)。
pub use container_runtime_api::DiagCtx;

#[allow(unused_imports)]
pub use agent_mgmt_forward::{
    AgentMgmtForwardCtx, InstallAgentParams, check_agent, get_agent, install_agent, list_agents,
    status_to_app_error, uninstall_agent,
};
#[allow(unused_imports)]
pub use grpc_addr::{container_identity_from_name, extract_grpc_addr, extract_grpc_addr_with_port};
pub use i18n_extractors::{I18nJsonOrQuery, I18nPath, I18nQuery};
pub use locale::get_locale_from_headers;
pub use paths::{
    COMPUTER_WORKSPACE_ROOT, build_computer_workspace_path, build_workspace_path, project_dir,
    user_dir,
};

// 内部使用：路径验证（通过路径函数自动调用，无需外部直接使用）
#[allow(unused_imports)]
pub use paths::{PathValidationError, is_known_identifier, validate_identifier};

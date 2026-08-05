//! Handler 工具模块
//!
//! 提供 handler 层共享的工具函数和常量。

mod agent_mgmt_forward;
mod grpc_addr;
mod i18n_extractors;
mod locale;
mod paths;

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

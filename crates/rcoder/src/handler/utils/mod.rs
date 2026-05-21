//! Handler 工具模块
//!
//! 提供 handler 层共享的工具函数和常量。

mod grpc_addr;
mod i18n_extractors;
mod locale;
mod paths;

pub use grpc_addr::{
    container_identity_from_name, extract_grpc_addr, extract_grpc_addr_with_port,
    get_realtime_container_ip,
};
pub use i18n_extractors::{I18nJsonOrQuery, I18nPath, I18nQuery};
pub use locale::get_locale_from_headers;
pub use paths::{
    COMPUTER_WORKSPACE_ROOT, build_computer_workspace_path, build_workspace_path, project_dir,
    user_dir,
};

//! Handler 工具模块
//!
//! 提供 handler 层共享的工具函数和常量。

mod grpc_addr;
mod locale;
mod paths;

pub use grpc_addr::{
    extract_grpc_addr, extract_grpc_addr_with_port, get_realtime_container_ip_with_cache,
};
pub use locale::get_locale_from_headers;
pub use paths::{COMPUTER_WORKSPACE_ROOT, project_dir, user_dir};

//! 工具函数模块
//!
//! 重新导出 docker_manager 路径解析 + 引擎侧共享工具（agent 诊断 / gRPC 地址 /
//! workspace 路径——自 rcoder handler/utils 迁入，grpc/cleanup_task/service 等
//! 引擎模块消费；rcoder 的 handler::utils 对这三族 re-export 保持兼容）。

pub mod agent_diagnostic;
pub mod grpc_addr;
pub mod paths;
pub use paths::workspace_root_path;

// 重新导出 docker_manager 的路径解析实现
#[allow(unused_imports)] // 导出供外部使用
pub use docker_manager::path::{HostPathResolver, resolve_container_path_to_host};

pub use agent_diagnostic::{
    build_connection_error, diagnose, root_cause_message, wait_agent_ready,
};
// DiagCtx 定义在共享 crate container-runtime-api，这里 re-export 供引擎内部统一引用
pub use container_runtime_api::DiagCtx;
pub use grpc_addr::{container_identity_from_name, extract_grpc_addr, extract_grpc_addr_with_port};
pub use paths::{
    COMPUTER_WORKSPACE_ROOT, build_computer_workspace_path, build_workspace_path,
    map_container_work_dir_to_host, project_dir, user_dir,
};

// 内部使用：路径验证（通过路径函数自动调用，无需外部直接使用）
#[allow(unused_imports)]
pub use paths::{PathValidationError, is_known_identifier, validate_identifier};

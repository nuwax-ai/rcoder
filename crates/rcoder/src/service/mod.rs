//! 服务模块
//!
//! 提供容器管理功能
//!
//! ## 模块说明
//! - `container_manager`: RCoder 模式的容器管理（project_id -> 容器）
//! - `computer_container_manager`: ComputerAgentRunner 模式的容器管理（user_id -> 容器）
//! - `container_status_checker`: 容器状态检查器（定期查询容器状态，防止误杀）

pub mod computer_container_manager;
pub mod container_manager;
pub mod container_status_checker;

pub use computer_container_manager::ComputerContainerManager;
pub use container_manager::*;
pub use container_status_checker::{start_container_status_checker, ContainerStatusCheckerConfig};

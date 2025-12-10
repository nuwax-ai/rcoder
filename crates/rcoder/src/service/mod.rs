//! 服务模块
//!
//! 提供容器管理功能
//!
//! ## 模块说明
//! - `container_manager`: RCoder 模式的容器管理（project_id -> 容器）
//! - `computer_container_manager`: ComputerAgentRunner 模式的容器管理（user_id -> 容器）

pub mod computer_container_manager;
pub mod container_manager;

pub use computer_container_manager::ComputerContainerManager;
pub use container_manager::*;

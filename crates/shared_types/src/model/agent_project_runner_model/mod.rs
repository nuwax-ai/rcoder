//! 项目与容器状态模型（目录化拆分；函数体原样搬迁，扁平 API 经 glob re-export 保持）。
//! - container_info: 独立 DTO（ContainerBasicInfo）
//! - state: 状态内核（Core/Extended/ProjectState 的 CoW 机制）
//! - facade: 兼容门面（ProjectAndContainerInfo，旧 API 桥接）

mod container_info;
mod facade;
mod state;

pub use container_info::*;
pub use facade::*;
pub use state::*;

//! Docker 管理域共享类型（按域分组；lib.rs `pub use types::*` glob 穿透，
//! `docker_manager::X` 与 `crate::types::X` 双路径均不变）。

mod cleanup;
mod config;
mod container;
mod status;

pub use cleanup::*;
pub use config::*;
pub use container::*;
pub use status::*;

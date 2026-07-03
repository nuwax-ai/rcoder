//! Agent Management (P0-1)
//!
//! 在 agent_runner 容器内提供 agent 二进制安装/卸载/检查能力。
//! 详见 `docs/acp-agent-management-api.md`。
//!
//! ## 子模块
//! - [`registry`] — 注册表(内存 + JSON 持久化)
//! - [`path_manager`] — 安装目录与 PATH 注入
//! - [`checker`] — agent 健康/版本/可执行性检查
//! - [`uninstaller`] — 卸载(拒绝 builtin)
//! - [`conversion`] — AgentManifest ↔ proto
//! - [`installer`] — 4 种安装方式(binary / npm / url / archive)
//! - [`error`] — 统一错误类型
//! - [`grpc`] — gRPC AgentMgmtService 实现

pub mod checker;
pub mod conversion;
pub mod error;
pub mod grpc;
pub mod install_lock;
pub mod installer;
pub mod path_manager;
pub mod registry;
pub mod uninstaller;

pub use error::AgentMgmtResult;
pub use install_lock::InstallLockManager;
pub use path_manager::PathManager;
pub use registry::AgentRegistry;

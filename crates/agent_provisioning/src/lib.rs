//! Agent provisioning 共享模块
//!
//! 提供 agent 的下载缓存、解压安装、registry 更新，供 rcoder（主安装路径）
//! 与 agent_runner（bundle 缺失兜底自装）复用。叶子 crate，无内部 crate 依赖。

pub mod error;
pub mod install;
pub mod manager;
pub mod registry_update;

pub use error::AgentDownloadError;
pub use install::{install_agent, is_agent_installed};
pub use manager::{AgentDownloadManager, DownloadResult};
pub use registry_update::{AgentManifest, update_registry};

//! Session 管理模块
//!
//! 使用 SACP (symposium-acp) 协议提供会话的统一管理能力，包括：
//! - 会话信息存储和查询
//! - 会话生命周期管理
//! - 模型配置变化检测
//! - Agent Worker 抽象
//!
//! ## 架构说明
//!
//! 会话信息统一使用 `ProjectAndAgentInfo`（定义在 `shared_types`），
//! 通过 `SessionEntry` trait 抽象访问接口。
//! 会话存储通过 `SessionRegistry` trait 抽象，允许注入不同实现（如 `AGENT_REGISTRY`）。

mod session_file_scanner;
mod worker;
mod sacp_session_manager;
mod sacp_worker;

pub use session_file_scanner::check_session_file_exists;
pub use worker::{AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};
pub use sacp_session_manager::SacpSessionManager;
pub use sacp_worker::SacpAgentWorker;

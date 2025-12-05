//! Session 管理模块
//!
//! 提供 ACP 会话的统一管理能力，包括：
//! - 会话信息存储和查询
//! - 会话生命周期管理
//! - 模型配置变化检测
//! - Agent Worker 抽象

mod acp_worker;
mod session_info;
mod session_manager;
mod worker;

pub use acp_worker::AcpAgentWorker;
pub use session_info::SessionInfo;
pub use session_manager::AcpSessionManager;
pub use worker::{AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};

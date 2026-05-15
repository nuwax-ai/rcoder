//! # Agent 启动器模块
//!
//! 提供 Agent 的启动、生命周期管理和通道处理功能。
//!
//! ## SACP 迁移说明
//!
//! 本模块已迁移至 SACP (Symposium ACP) 实现，主要优势：
//! - **支持 Send trait**: 可使用标准 `tokio::spawn`，无需 `LocalSet` + `spawn_local`
//! - **Builder 模式**: 更清晰的连接构建和配置
//! - **回调式消息处理**: 通过 `on_receive_notification` / `on_receive_request` 宏
//!
//! ## 子模块职责
//!
//! | 子模块 | 职责 | 关键类型 |
//! |--------|------|---------|
//! | [`lifecycle`] | Agent 生命周期守卫（RAII 资源管理）| `AgentLifecycleGuard` |
//! | [`claude_code_sacp`] | SACP 版本的 Claude Code Agent 启动器 | `SacpClaudeCodeLauncher` |
//!
//! ## Agent 启动流程 (SACP)
//!
//! ```text
//! AcpSessionManager.create_session()
//!       │
//!       │ 1. 创建 SacpClaudeCodeLauncher
//!       ▼
//! SacpClaudeCodeLauncher.launch()
//!       │
//!       │ 2. 启动 Claude Code 子进程
//!       │ 3. 使用 SACP Builder 建立连接
//!       │ 4. 创建 AgentLifecycleGuard
//!       ▼
//! SacpLauncherConnectionInfo
//!       │
//!       │ 5. 返回 session_id, prompt_tx, cancel_tx, lifecycle_guard
//!       ▼
//! 存入 SessionRegistry
//! ```
//!
//! ## 与 shared_types::AgentLifecycleGuard 的关系
//!
//! `AgentLifecycleGuard` 的核心实现定义在 `shared_types::model::agent_model`。
//! 本模块的 `lifecycle.rs` 提供 Re-export，
//! 方便从 `agent_abstraction::launcher` 统一导入。
//!
//! ## 生命周期管理 (RAII)
//!
//! 当 `AgentLifecycleGuard` 被 drop 时：
//! 1. 发送取消信号 (`cancel_token.cancel()`)
//! 2. 终止子进程 (`child.kill()`)
//! 3. 停止 stderr 任务
//!
//! 这确保了 Agent 资源的正确清理，即使在异常情况下也不会泄漏。

mod claude_code_sacp;
pub mod lifecycle;
pub mod model_env;
#[cfg(windows)]
mod windows_launch;

// ============================================================================
// SACP 类型导出（推荐使用）
// ============================================================================

pub use lifecycle::AgentLifecycleGuard;
pub use model_env::{
    DirectModelRuntimeEnvResolver, ModelRuntimeEnvResolver, ProxyModelRuntimeEnvResolver,
    ResolvedModelEnv, direct_model_runtime_env_resolver,
};

// 直接导出 SACP 类型
pub use claude_code_sacp::{
    SacpAgentLaunchConfig, SacpClaudeCodeLauncher, SacpLauncherConnectionInfo,
    convert_context_servers_sacp, get_default_sacp_agent_config, load_sacp_agent_config,
};

// ============================================================================
// 兼容性类型别名（向后兼容旧代码）
// ============================================================================

/// 兼容性别名：ClaudeCodeLauncher -> SacpClaudeCodeLauncher
pub type ClaudeCodeLauncher<N> = SacpClaudeCodeLauncher<N>;

/// 兼容性别名：LauncherConnectionInfoComplete -> SacpLauncherConnectionInfo
pub type LauncherConnectionInfoComplete = SacpLauncherConnectionInfo;

/// 兼容性别名：AgentLaunchConfig -> SacpAgentLaunchConfig
pub type AgentLaunchConfig = SacpAgentLaunchConfig;

/// 兼容性别名：load_agent_config -> load_sacp_agent_config
pub use claude_code_sacp::load_sacp_agent_config as load_agent_config;

/// 兼容性别名：get_default_agent_config -> get_default_sacp_agent_config
pub use claude_code_sacp::get_default_sacp_agent_config as get_default_agent_config;

/// 兼容性别名：convert_context_servers -> convert_context_servers_sacp
pub use claude_code_sacp::convert_context_servers_sacp as convert_context_servers;

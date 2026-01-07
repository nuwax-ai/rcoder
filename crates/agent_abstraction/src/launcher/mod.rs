//! # Agent 启动器模块
//!
//! 提供 Agent 的启动、生命周期管理和通道处理功能。
//!
//! ## 子模块职责
//!
//! | 子模块 | 职责 | 关键类型 |
//! |--------|------|---------|
//! | [`lifecycle`] | Agent 生命周期守卫（RAII 资源管理）| `AgentLifecycleGuard` |
//! | `claude_code` | Claude Code Agent 启动器 | `ClaudeCodeLauncher` |
//! | `channel` | Prompt/Cancel 通道处理工具 | `spawn_prompt_handler_for_agent` |
//!
//! ## Agent 启动流程
//!
//! ```text
//! AcpSessionManager.create_session()
//!       │
//!       │ 1. 创建 ClaudeCodeLauncher
//!       ▼
//! ClaudeCodeLauncher.launch()
//!       │
//!       │ 2. 启动 Claude Code 子进程
//!       │ 3. 建立 ACP 连接 (stdin/stdout)
//!       │ 4. 创建 AgentLifecycleGuard
//!       ▼
//! LauncherConnectionInfo
//!       │
//!       │ 5. 返回 session_id, prompt_tx, cancel_tx, lifecycle_guard
//!       ▼
//! 存入 SessionRegistry
//! ```
//!
//! ## 与 shared_types::AgentLifecycleGuard 的关系
//!
//! `AgentLifecycleGuard` 的核心实现定义在 `shared_types::model::agent_model`。
//! 本模块的 `lifecycle.rs` 提供：
//!
//! - **类型别名**: `AgentStopGuard`, `AgentStopHandleArc`
//! - **Re-export**: 方便从 `agent_abstraction::launcher` 统一导入
//!
//! ## 生命周期管理 (RAII)
//!
//! 当 `AgentLifecycleGuard` 被 drop 时：
//! 1. 发送取消信号 (`cancel_token.cancel()`)
//! 2. 终止子进程 (`child.kill()`)
//! 3. 停止 stderr 任务
//!
//! 这确保了 Agent 资源的正确清理，即使在异常情况下也不会泄漏。
//!
//! ## 配置加载
//!
//! - `get_default_agent_config()` - 获取默认 Agent 配置
//! - `load_agent_config()` - 从配置文件加载 Agent 配置
//! - `convert_context_servers()` - 转换 MCP 服务器配置

pub mod lifecycle;
mod channel;
mod claude_code;

pub use lifecycle::{AgentLifecycleGuard, AgentStopGuard, AgentStopHandleArc};
pub use channel::{
    PromptHandlerConfig, spawn_cancel_handler_for_agent, spawn_prompt_handler_for_agent,
};
pub use claude_code::{
    AgentLaunchConfig, ClaudeCodeLauncher, LauncherConnectionInfo, LauncherConnectionInfoComplete,
    convert_context_servers, get_default_agent_config, load_agent_config,
};

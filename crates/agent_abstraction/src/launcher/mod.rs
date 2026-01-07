//! # Agent 启动器模块
//!
//! 提供 Agent 的启动、生命周期管理和通道处理功能。
//!
//! 使用 SACP (symposium-acp) 协议，支持标准 tokio::spawn（无需 LocalSet）。
//!
//! ## 子模块职责
//!
//! | 子模块 | 职责 | 关键类型 |
//! |--------|------|---------|
//! | [`lifecycle`] | Agent 生命周期守卫（RAII 资源管理）| `AgentLifecycleGuard` |
//! | `claude_code_sacp` | Claude Code Agent 启动器（SACP 版本）| `SacpClaudeCodeLauncher` |
//!
//! ## Agent 启动流程
//!
//! ```text
//! SacpSessionManager.create_session()
//!       │
//!       │ 1. 创建 SacpClaudeCodeLauncher
//!       ▼
//! SacpClaudeCodeLauncher.launch()
//!       │
//!       │ 2. 启动 Claude Code 子进程
//!       │ 3. 建立 SACP 连接 (stdin/stdout)
//!       │ 4. 创建 AgentLifecycleGuard
//!       ▼
//! SacpLauncherConnectionInfo
//!       │
//!       │ 5. 返回 session_id, prompt_tx, cancel_tx, lifecycle_guard
//!       ▼
//! 存入 SessionRegistry
//! ```
//!
//! ## 生命周期管理 (RAII)
//!
//! 当 `AgentLifecycleGuard` 被 drop 时：
//! 1. 发送取消信号 (`cancel_token.cancel()`)
//! 2. 终止子进程 (`child.kill()`)
//! 3. 停止 stderr 任务
//!
//! ## 配置加载
//!
//! - `get_default_sacp_agent_config()` - 获取默认 Agent 配置
//! - `load_sacp_agent_config()` - 从配置文件加载 Agent 配置
//! - `convert_context_servers_sacp()` - 转换 MCP 服务器配置

pub mod lifecycle;
mod claude_code_sacp;

pub use lifecycle::{AgentLifecycleGuard, AgentStopGuard, AgentStopHandleArc};

// SACP 版本的启动器（唯一实现）
pub use claude_code_sacp::{
    SacpAgentLaunchConfig, SacpClaudeCodeLauncher, SacpLauncherConnectionInfo,
    convert_context_servers_sacp, get_default_sacp_agent_config, load_sacp_agent_config,
};

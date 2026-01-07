//! Agent 启动器模块
//!
//! 提供 Agent 的启动、生命周期管理和通道处理功能：
//! - `lifecycle` - Agent 生命周期守卫（RAII 资源管理）
//! - `claude_code` - Claude Code Agent 启动器
//! - `channel` - Prompt/Cancel 通道处理工具

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

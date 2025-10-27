//! Codex AI Agent Library
//!
//! 提供基于 OpenAI Codex 的 AI 代理服务，通过 ACP (Agent Client Protocol) 协议实现
//! 与 OpenAI API 的集成。

// 重新导出安装模块
pub mod install;

// 由于 codex-acp 没有公开导出 CodexAgent 和相关类型，
// 我们需要使用子进程方式，而不是嵌入式方式
// 因此这里只导出公共的 API 类型
pub use codex_acp::{
    CodexToolCallParam, CodexToolCallReplyParam, 
    ExecApprovalElicitRequestParams, ExecApprovalResponse,
    PatchApprovalElicitRequestParams, PatchApprovalResponse,
};


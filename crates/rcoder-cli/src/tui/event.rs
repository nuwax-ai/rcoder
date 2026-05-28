//! TUI 应用事件类型
//!
//! 统一事件枚举，汇聚来自终端、Agent、内部逻辑的所有事件。
//! 参考 codex 的 `AppEvent` 设计，简化为单通道 mpsc。

use crossterm::event::KeyEvent;

/// Prompt 代际标识
///
/// 每次用户提交新 prompt 时递增。Spawned task 捕获提交时的 generation，
/// 在发送 ResetWaiting 时附带，事件循环只在 generation 匹配时处理。
/// 解决 stale event 竞态：旧 prompt 的超时/错误事件不会影响新 prompt。
pub type PromptGeneration = u64;

/// TUI 应用事件
#[derive(Debug)]
#[allow(dead_code)]
pub enum AppEvent {
    // ── 终端事件（来自 crossterm） ──

    /// 键盘按键
    Key(KeyEvent),

    /// 终端窗口大小变化
    Resize,

    /// 粘贴内容（bracketed paste）
    Paste(String),

    // ── Agent 事件（来自 TuiSessionNotifier） ──

    /// Agent 流式文本片段
    AgentText(String),

    /// Agent 思考内容（verbose 模式显示）
    AgentThought(String),

    /// 工具调用状态更新
    ToolCall { title: String, status: String },

    /// Prompt 开始处理
    PromptStarted { session_id: String },

    /// Prompt 处理结束
    PromptEnded {
        session_id: String,
        error: Option<String>,
    },

    /// 权限请求（来自 TuiPermissionPrompt）
    PermissionRequest {
        request_id: usize,
        tool_name: String,
        options: Vec<PermissionOption>,
        response_tx: tokio::sync::oneshot::Sender<Option<String>>,
    },

    /// 诊断信息（进程生命周期事件）
    Diagnostics(String),

    // ── 内部事件 ──

    /// 重置 waiting 状态（prompt 失败但 Agent 未发送 PromptEnded 时使用）。
    /// 携带 generation 标识，仅当 generation 匹配当前 prompt 时才生效，
    /// 防止旧 prompt 的超时错误覆盖新 prompt 的状态。
    ResetWaiting(PromptGeneration),

    /// 请求退出
    Exit,
}

/// 权限选项
#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
}

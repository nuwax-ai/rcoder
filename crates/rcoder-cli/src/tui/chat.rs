//! 聊天状态管理
//!
//! 维护对话消息列表、当前流式响应、滚动偏移等状态。

use ratatui::text::Text;

use crate::tui::markdown::render_markdown;

/// 聊天消息角色
#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Agent,
    System,
    Tool,
}

/// 单条聊天消息
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Agent 消息的 Markdown 渲染缓存
    ///
    /// 在 `commit_response` 时渲染一次，后续 `draw` 直接使用缓存。
    /// 非 Agent 消息为 `None`（它们使用简单的行样式，不需要 Markdown 解析）。
    pub rendered: Option<Text<'static>>,
}

/// 聊天状态
pub struct ChatState {
    /// 已完成的消息历史
    pub messages: Vec<ChatMessage>,
    /// 当前正在流式接收的 agent 文本
    pub current_response: String,
    /// 滚动偏移量（0 = 最新位置）
    pub scroll_offset: u16,
    /// 是否正在等待 agent 响应
    pub waiting: bool,
    /// 是否自动滚动到底部跟踪最新内容
    ///
    /// 用户手动向上滚动时设为 false，此时新 token 到达不强制回底，
    /// 而是补偿 scroll_offset 保持视口稳定。滚回底部时自动恢复 true。
    pub auto_scroll: bool,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            current_response: String::new(),
            scroll_offset: 0,
            waiting: false,
            auto_scroll: true,
        }
    }

    /// 添加用户消息
    pub fn push_user_message(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            role: Role::User,
            content: text.to_string(),
            rendered: None,
        });
        self.scroll_offset = 0;
        self.auto_scroll = true; // 用户自己的消息始终跟踪底部
    }

    /// 追加流式 agent 文本
    ///
    /// `auto_scroll` 开启时跟踪底部；关闭时补偿 `scroll_offset`
    /// 使视口保持对用户正在查看的内容稳定（新行在视口下方增加）。
    pub fn push_agent_text(&mut self, chunk: &str) {
        if self.auto_scroll {
            self.current_response.push_str(chunk);
            self.scroll_offset = 0;
        } else {
            // 计算新增行数，补偿 scroll_offset 保持视口位置不变
            let newlines = chunk.chars().filter(|&c| c == '\n').count();
            self.current_response.push_str(chunk);
            self.scroll_offset = self.scroll_offset.saturating_add(newlines as u16);
        }
    }

    /// 添加工具调用消息
    pub fn push_tool_call(&mut self, title: &str, status: &str) {
        self.messages.push(ChatMessage {
            role: Role::Tool,
            content: format!("[{}] {}", title, status),
            rendered: None,
        });
    }

    /// 添加系统消息
    pub fn push_system_message(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            role: Role::System,
            content: text.to_string(),
            rendered: None,
        });
    }

    /// 提交当前响应到消息历史
    ///
    /// `use_markdown` 为 true 时立即渲染 Markdown 并缓存到 `rendered` 字段，
    /// 后续 `draw` 直接使用缓存，避免每帧重新解析所有历史消息。
    pub fn commit_response(&mut self, use_markdown: bool) {
        if !self.current_response.is_empty() {
            let text = std::mem::take(&mut self.current_response);
            let rendered = if use_markdown {
                Some(render_markdown(&text))
            } else {
                None
            };
            self.messages.push(ChatMessage {
                role: Role::Agent,
                content: text,
                rendered,
            });
        }
        self.waiting = false;
    }

    /// 向上滚动
    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        self.auto_scroll = false; // 用户手动滚动，关闭自动跟踪
    }

    /// 向下滚动
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            self.auto_scroll = true; // 滚回底部，恢复自动跟踪
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_defaults() {
        let s = ChatState::new();
        assert!(s.messages.is_empty());
        assert!(s.current_response.is_empty());
        assert_eq!(s.scroll_offset, 0);
        assert!(!s.waiting);
        assert!(s.auto_scroll);
    }

    #[test]
    fn push_user_message_resets_scroll() {
        let mut s = ChatState::new();
        s.scroll_offset = 10;
        s.auto_scroll = false;

        s.push_user_message("hello");
        assert_eq!(s.scroll_offset, 0);
        assert!(s.auto_scroll);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].content, "hello");
    }

    #[test]
    fn push_agent_text_auto_scroll_keeps_offset_zero() {
        let mut s = ChatState::new();
        s.auto_scroll = true;

        s.push_agent_text("hello\n");
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.current_response, "hello\n");

        s.push_agent_text("world");
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.current_response, "hello\nworld");
    }

    #[test]
    fn push_agent_text_manual_scroll_compensates() {
        let mut s = ChatState::new();
        s.auto_scroll = false;
        s.scroll_offset = 5;

        s.push_agent_text("line1\nline2\nline3\n");
        // 3 newlines → compensate by +3
        assert_eq!(s.scroll_offset, 8);
        assert_eq!(s.current_response, "line1\nline2\nline3\n");
    }

    #[test]
    fn push_agent_text_manual_scroll_no_newlines() {
        let mut s = ChatState::new();
        s.auto_scroll = false;
        s.scroll_offset = 5;

        s.push_agent_text("hello");
        // no newlines → offset unchanged
        assert_eq!(s.scroll_offset, 5);
    }

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let mut s = ChatState::new();
        assert!(s.auto_scroll);

        s.scroll_up(3);
        assert_eq!(s.scroll_offset, 3);
        assert!(!s.auto_scroll);
    }

    #[test]
    fn scroll_down_restores_auto_scroll_at_bottom() {
        let mut s = ChatState::new();
        s.scroll_up(10);
        assert!(!s.auto_scroll);

        s.scroll_down(5);
        assert_eq!(s.scroll_offset, 5);
        assert!(!s.auto_scroll); // not yet at bottom

        s.scroll_down(5);
        assert_eq!(s.scroll_offset, 0);
        assert!(s.auto_scroll); // reached bottom
    }

    #[test]
    fn commit_response_caches_markdown() {
        let mut s = ChatState::new();
        s.push_agent_text("# Hello");

        s.commit_response(true);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].role, Role::Agent);
        assert_eq!(s.messages[0].content, "# Hello");
        assert!(s.messages[0].rendered.is_some()); // cached
        assert!(!s.waiting);
    }

    #[test]
    fn commit_response_no_markdown() {
        let mut s = ChatState::new();
        s.push_agent_text("plain text");

        s.commit_response(false);
        assert!(s.messages[0].rendered.is_none()); // not cached
    }

    #[test]
    fn commit_response_empty_is_noop() {
        let mut s = ChatState::new();
        s.waiting = true;

        s.commit_response(true);
        assert!(s.messages.is_empty()); // no message added
        assert!(!s.waiting); // but waiting is cleared
    }

    #[test]
    fn push_tool_call_and_system() {
        let mut s = ChatState::new();
        s.push_tool_call("Read", "done");
        s.push_system_message("error occurred");

        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::Tool);
        assert_eq!(s.messages[0].content, "[Read] done");
        assert_eq!(s.messages[1].role, Role::System);
        assert_eq!(s.messages[1].content, "error occurred");
    }

    #[test]
    fn scroll_up_saturates() {
        let mut s = ChatState::new();
        s.scroll_up(u16::MAX);
        assert_eq!(s.scroll_offset, u16::MAX);

        s.scroll_up(10);
        assert_eq!(s.scroll_offset, u16::MAX); // saturating_add
    }

    #[test]
    fn scroll_down_saturates() {
        let mut s = ChatState::new();
        s.scroll_offset = 5;
        s.scroll_down(100);
        assert_eq!(s.scroll_offset, 0); // saturating_sub
        assert!(s.auto_scroll);
    }
}

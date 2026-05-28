//! TUI 布局与渲染
//!
//! 三面板布局：StatusBar / ChatArea / Composer
//! 支持权限弹窗覆盖层
//!
//! 注：渲染函数接收独立字段而非 `&App`，以解决 `terminal.draw()` 的借用冲突。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::tui::app::PermissionOverlay;
use crate::tui::chat::{ChatState, Role};
use crate::tui::composer::Composer;
use crate::tui::markdown::render_markdown;

/// Composer 空状态占位提示
const COMPOSER_PLACEHOLDER: &str = "Type a message... (Enter=send, Alt+Enter=newline, Esc=quit)";

/// 按字符数截断字符串，超出部分用 "..." 替换。
/// 安全处理 UTF-8 多字节字符（不会在字符中间截断）。
fn truncate_display(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

/// 渲染整个 TUI 界面
///
/// 接收 App 的各个独立字段，避免 `self.terminal` 与 `self` 的借用冲突。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    chat: &ChatState,
    composer: &Composer,
    pending_prompt: Option<&str>,
    permission_overlay: Option<&PermissionOverlay>,
    use_markdown: bool,
    project_id: &str,
    session_id: &str,
) {
    let area = frame.area();
    let composer_height = composer.display_height();

    let chunks = Layout::vertical([
        Constraint::Length(1),                  // StatusBar
        Constraint::Min(5),                     // ChatArea
        Constraint::Length(composer_height),    // Composer (dynamic)
    ])
    .split(area);

    draw_status_bar(frame, chat, project_id, session_id, chunks[0]);
    draw_chat_area(frame, chat, use_markdown, chunks[1]);
    draw_composer(frame, chat, composer, pending_prompt, chunks[2]);

    // 权限弹窗覆盖层
    if let Some(overlay) = permission_overlay {
        draw_permission_overlay(frame, overlay, area);
    }
}

/// 渲染状态栏
fn draw_status_bar(
    frame: &mut Frame,
    chat: &ChatState,
    project_id: &str,
    session_id: &str,
    area: Rect,
) {
    let status = if chat.waiting {
        Span::styled("Running", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("Idle", Style::default().fg(Color::Green))
    };

    let project_display = truncate_display(project_id, 12);
    let session_display = truncate_display(session_id, 8);

    let spans = vec![
        Span::styled(
            " rcoder-cli ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", project_display),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(
            format!(" {} ", session_display),
            Style::default().fg(Color::Gray).bg(Color::DarkGray),
        ),
        Span::raw(" "),
        status,
    ];

    frame.render_widget(Line::from(spans), area);
}

/// 渲染聊天区域
fn draw_chat_area(frame: &mut Frame, chat: &ChatState, use_markdown: bool, area: Rect) {
    let block = Block::default().borders(Borders::NONE);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height as usize;
    if visible_height == 0 {
        return;
    }

    let mut all_lines: Vec<Line<'static>> = Vec::new();

    // 渲染已完成的消息
    for msg in &chat.messages {
        match msg.role {
            Role::User => {
                all_lines.push(Line::from(Span::styled(
                    "> ".to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                for line in msg.content.lines() {
                    all_lines.push(Line::from(Span::styled(
                        format!("  {}", line),
                        Style::default().fg(Color::Cyan),
                    )));
                }
                all_lines.push(Line::from(""));
            }
            Role::Agent => {
                if let Some(ref rendered) = msg.rendered {
                    // 使用缓存的 Markdown 渲染结果
                    all_lines.extend(rendered.lines.clone());
                } else if use_markdown {
                    // Fallback: 未缓存时现场渲染（不应发生，commit_response 会缓存）
                    let text = render_markdown(&msg.content);
                    all_lines.extend(text.lines);
                } else {
                    for line in msg.content.lines() {
                        all_lines.push(Line::from(line.to_string()));
                    }
                }
                all_lines.push(Line::from(""));
            }
            Role::Tool => {
                all_lines.push(Line::from(Span::styled(
                    format!("  [tool] {}", msg.content),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Role::System => {
                all_lines.push(Line::from(Span::styled(
                    format!("  [sys] {}", msg.content),
                    Style::default().fg(Color::Blue),
                )));
            }
        }
    }

    // 渲染当前流式响应
    if !chat.current_response.is_empty() {
        if use_markdown {
            let text = render_markdown(&chat.current_response);
            all_lines.extend(text.lines);
        } else {
            for line in chat.current_response.lines() {
                all_lines.push(Line::from(line.to_string()));
            }
        }
        // 流式光标
        all_lines.push(Line::from(Span::styled(
            "  ▌".to_string(),
            Style::default().fg(Color::Gray),
        )));
    }

    // 等待提示
    if chat.waiting && chat.current_response.is_empty() {
        all_lines.push(Line::from(Span::styled(
            "  ...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // 计算可见窗口：scroll_offset=0 表示显示最新内容
    // Clamp offset 防止超出范围导致白屏
    let total = all_lines.len();
    let max_offset = total.saturating_sub(visible_height);
    let offset = (chat.scroll_offset as usize).min(max_offset);
    let end = total.saturating_sub(offset);
    let start = end.saturating_sub(visible_height);
    let actual_end = end.min(start + visible_height);

    let visible: Vec<Line<'static>> = all_lines
        .into_iter()
        .skip(start)
        .take(actual_end.saturating_sub(start))
        .collect();

    let paragraph = Paragraph::new(Text::from(visible));
    frame.render_widget(paragraph, inner);
}

/// 渲染输入框（支持多行）
fn draw_composer(
    frame: &mut Frame,
    chat: &ChatState,
    composer: &Composer,
    pending_prompt: Option<&str>,
    area: Rect,
) {
    let status_indicator = if pending_prompt.is_some() {
        " [Queued] "
    } else if chat.waiting {
        " [Running...] "
    } else {
        ""
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(format!(" Prompt{} ", status_indicator))
        .title_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // 空输入时显示占位提示
    if composer.is_empty() {
        let placeholder = Line::from(Span::styled(
            COMPOSER_PLACEHOLDER,
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(placeholder), inner);
        frame.set_cursor_position((inner.x, inner.y));
        return;
    }

    // 渲染多行内容
    let lines: Vec<Line> = composer.lines.iter().map(|l| Line::from(l.as_str())).collect();
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });

    // 如果光标行超出可见区域，滚动段落使其可见
    let visible_height = inner.height as usize;
    let scroll_top = if composer.cursor_row >= visible_height {
        (composer.cursor_row - visible_height + 1) as u16
    } else {
        0
    };
    let paragraph = paragraph.scroll((scroll_top, 0));
    frame.render_widget(paragraph, inner);

    // 计算光标显示位置：当前行的显示列 + 可见行号
    let row_in_view = composer.cursor_row as u16 - scroll_top;
    let display_col: u16 = composer.lines[composer.cursor_row]
        .chars()
        .take(composer.cursor_col)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1) as u16)
        .sum();

    frame.set_cursor_position((inner.x + display_col, inner.y + row_in_view));
}

/// 渲染权限弹窗
fn draw_permission_overlay(frame: &mut Frame, overlay: &PermissionOverlay, area: Rect) {
    // 背景遮罩
    frame.render_widget(Clear, area);

    let overlay_bg = Block::default().style(Style::default().bg(Color::Rgb(20, 20, 20)));
    frame.render_widget(overlay_bg, area);

    // 计算弹窗尺寸（居中）
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = (overlay.options.len() as u16 + 6).min(area.height.saturating_sub(4));
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Permission Request ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Rgb(30, 30, 30)));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::raw("Tool: "),
            Span::styled(
                overlay.tool_name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    for (i, opt) in overlay.options.iter().enumerate() {
        let style = if i == overlay.selected_index {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("  [{}] {}", i + 1, opt.label),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter=select  j/k=navigate  Esc=cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(Text::from(lines));
    frame.render_widget(paragraph, inner);
}

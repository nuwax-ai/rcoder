//! 终端权限确认提示
//!
//! 实现 `PermissionPrompt` trait，在终端中显示权限请求并等待用户确认。

use std::io::IsTerminal;

use agent_abstraction::{PermissionPrompt, PermissionRequestContext};
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use anyhow::Result;
use async_trait::async_trait;

use crate::output::formatter::colors;

/// 终端权限确认提示
///
/// 当 Agent 请求权限时（如执行危险命令），在终端显示选项并等待用户输入。
/// 自动检测 stderr 是否为 TTY，非 TTY 时禁用 ANSI 颜色码。
pub struct TerminalPermissionPrompt {
    color: bool,
}

impl TerminalPermissionPrompt {
    pub fn new() -> Self {
        Self {
            color: std::io::stderr().is_terminal(),
        }
    }

    /// Render option kind to human-readable text
    fn render_option_kind(
        kind: &agent_client_protocol::schema::v1::PermissionOptionKind,
    ) -> &'static str {
        use agent_client_protocol::schema::v1::PermissionOptionKind;
        match kind {
            PermissionOptionKind::AllowOnce => "Allow once",
            PermissionOptionKind::AllowAlways => "Allow always",
            PermissionOptionKind::RejectOnce => "Deny (this time)",
            PermissionOptionKind::RejectAlways => "Deny (always)",
            _ => "Unknown",
        }
    }

    /// 输出带颜色或纯文本的权限框行
    fn print_border(&self, text: &str) {
        if self.color {
            eprintln!(
                "{}{}{}{}",
                colors::YELLOW,
                colors::BOLD,
                text,
                colors::RESET
            );
        } else {
            eprintln!("{}", text);
        }
    }

    /// 输出带黄色竖线前缀的行
    fn print_line(&self, content: &str) {
        if self.color {
            eprintln!("{}│{} {}", colors::YELLOW, colors::RESET, content);
        } else {
            eprintln!("│ {}", content);
        }
    }

    /// 输出带黄色竖线前缀的行（带粗体高亮部分）
    fn print_line_with_bold(&self, prefix: &str, bold_part: &str, suffix: &str) {
        if self.color {
            eprintln!(
                "{}│{} {}{}{}{}{}",
                colors::YELLOW,
                colors::RESET,
                prefix,
                colors::BOLD,
                bold_part,
                colors::RESET,
                suffix
            );
        } else {
            eprintln!("│ {}{}{}", prefix, bold_part, suffix);
        }
    }
}

impl Default for TerminalPermissionPrompt {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionPrompt for TerminalPermissionPrompt {
    async fn prompt_user(
        &self,
        _context: &PermissionRequestContext,
        request: &RequestPermissionRequest,
    ) -> Result<Option<String>> {
        let tool_name = request
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("unknown tool");

        eprintln!();
        self.print_border("┌─ Permission Request ─────────────────────────────┐");
        self.print_line_with_bold("Agent requests permission for: ", tool_name, "");
        self.print_line("");

        for (i, opt) in request.options.iter().enumerate() {
            let kind_text = Self::render_option_kind(&opt.kind);
            let label = if opt.name.is_empty() {
                kind_text.to_string()
            } else {
                format!("{} ({})", opt.name, kind_text)
            };
            self.print_line(&format!("  [{}] {}", i + 1, label));
        }
        self.print_line("");
        if self.color {
            eprint!(
                "{}│{} Enter choice (1-{}) or 'q' to cancel: ",
                colors::YELLOW,
                colors::RESET,
                request.options.len()
            );
        } else {
            eprint!(
                "│ Enter choice (1-{}) or 'q' to cancel: ",
                request.options.len()
            );
        }

        let input = tokio::task::spawn_blocking(|| {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok()?;
            Some(input.trim().to_string())
        })
        .await?;

        self.print_border("└──────────────────────────────────────────────────┘");
        eprintln!();

        let input = match input {
            Some(s) => s,
            None => return Ok(None),
        };

        match input.to_lowercase().as_str() {
            "q" | "quit" | "cancel" | "n" => Ok(None),
            "" => Ok(None),
            _ => match input.parse::<usize>() {
                Ok(n) if n >= 1 && n <= request.options.len() => {
                    let option_id = request.options[n - 1].option_id.0.to_string();
                    Ok(Some(option_id))
                }
                _ => {
                    if self.color {
                        eprintln!("{}Invalid choice: {}{}", colors::RED, input, colors::RESET);
                    } else {
                        eprintln!("Invalid choice: {}", input);
                    }
                    Ok(None)
                }
            },
        }
    }
}

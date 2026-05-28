//! 终端权限确认提示
//!
//! 实现 `PermissionPrompt` trait，在终端中显示权限请求并等待用户确认。

use agent_abstraction::{PermissionPrompt, PermissionRequestContext};
use agent_client_protocol::schema::RequestPermissionRequest;
use anyhow::Result;
use async_trait::async_trait;

/// 终端权限确认提示
///
/// 当 Agent 请求权限时（如执行危险命令），在终端显示选项并等待用户输入。
///
/// # 显示格式
///
/// ```text
/// ┌─ Permission Request ─────────────────────────────┐
/// │ Agent requests permission for: <tool_name>       │
/// │   [1] Allow once                                 │
/// │   [2] Allow always                               │
/// │   [3] Deny                                       │
/// │                                                  │
/// │ Enter choice (1-3) or 'q' to cancel:             │
/// └──────────────────────────────────────────────────┘
/// ```
pub struct TerminalPermissionPrompt;

impl TerminalPermissionPrompt {
    pub fn new() -> Self {
        Self
    }

    /// Render option kind to human-readable text
    fn render_option_kind(
        kind: &agent_client_protocol::schema::PermissionOptionKind,
    ) -> &'static str {
        use agent_client_protocol::schema::PermissionOptionKind;
        match kind {
            PermissionOptionKind::AllowOnce => "Allow once",
            PermissionOptionKind::AllowAlways => "Allow always",
            PermissionOptionKind::RejectOnce => "Deny (this time)",
            PermissionOptionKind::RejectAlways => "Deny (always)",
            _ => "Unknown",
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
        // Extract tool name from tool_call
        let tool_name = request
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("unknown tool");

        // Display the permission request
        eprintln!();
        eprintln!("\x1b[33m\x1b[1m┌─ Permission Request ─────────────────────────────┐\x1b[0m");
        eprintln!(
            "\x1b[33m│\x1b[0m Agent requests permission for: \x1b[1m{}\x1b[0m",
            tool_name
        );
        eprintln!("\x1b[33m│\x1b[0m");

        for (i, opt) in request.options.iter().enumerate() {
            let kind_text = Self::render_option_kind(&opt.kind);
            // Use opt.name if available, otherwise fall back to kind text
            let label = if opt.name.is_empty() {
                kind_text.to_string()
            } else {
                format!("{} ({})", opt.name, kind_text)
            };
            eprintln!("\x1b[33m│\x1b[0m   [{}] {}", i + 1, label);
        }
        eprintln!("\x1b[33m│\x1b[0m");
        eprint!(
            "\x1b[33m│\x1b[0m Enter choice (1-{}) or 'q' to cancel: ",
            request.options.len()
        );

        // Read user input (blocking in a spawned task to avoid blocking the async runtime)
        let input = tokio::task::spawn_blocking(|| {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok()?;
            Some(input.trim().to_string())
        })
        .await?;

        eprintln!(
            "\x1b[33m└──────────────────────────────────────────────────┘\x1b[0m"
        );
        eprintln!();

        let input = match input {
            Some(s) => s,
            None => return Ok(None), // EOF
        };

        // Parse input
        match input.to_lowercase().as_str() {
            "q" | "quit" | "cancel" | "n" => Ok(None),
            "" => Ok(None),
            _ => {
                // Try to parse as number
                match input.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= request.options.len() => {
                        // Convert PermissionOptionId to String
                        let option_id = request.options[n - 1].option_id.0.to_string();
                        Ok(Some(option_id))
                    }
                    _ => {
                        eprintln!("\x1b[31mInvalid choice: {}\x1b[0m", input);
                        Ok(None)
                    }
                }
            }
        }
    }
}

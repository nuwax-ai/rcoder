//! TUI 权限弹窗桥接
//!
//! 实现 PermissionPrompt trait，将权限请求通过 mpsc channel 转发到 TUI 事件循环，
//! 在 TUI 界面中显示为模态弹窗，用户选择后通过 oneshot channel 返回结果。

use std::sync::atomic::{AtomicUsize, Ordering};

use agent_abstraction::{PermissionPrompt, PermissionRequestContext};
use agent_client_protocol::schema::RequestPermissionRequest;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::tui::event::{AppEvent, PermissionOption};

/// TUI 权限弹窗桥接
///
/// 将权限请求转换为 AppEvent::PermissionRequest，由事件循环渲染为弹窗。
/// 用户选择结果通过 oneshot channel 返回。
pub struct TuiPermissionPrompt {
    tx: mpsc::UnboundedSender<AppEvent>,
    next_id: AtomicUsize,
}

impl TuiPermissionPrompt {
    pub fn new(tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            tx,
            next_id: AtomicUsize::new(0),
        }
    }

    /// 将 ACP PermissionOptionKind 转换为人类可读文本
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

#[async_trait]
impl PermissionPrompt for TuiPermissionPrompt {
    async fn prompt_user(
        &self,
        _context: &PermissionRequestContext,
        request: &RequestPermissionRequest,
    ) -> Result<Option<String>> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let options: Vec<PermissionOption> = request
            .options
            .iter()
            .map(|opt| {
                let kind_text = Self::render_option_kind(&opt.kind);
                let label = if opt.name.is_empty() {
                    kind_text.to_string()
                } else {
                    format!("{} ({})", opt.name, kind_text)
                };
                PermissionOption {
                    id: opt.option_id.0.to_string(),
                    label,
                }
            })
            .collect();

        let tool_name = request
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("unknown tool")
            .to_string();

        let (resp_tx, resp_rx) = oneshot::channel();

        // 发送权限请求到 TUI 事件循环
        let _ = self.tx.send(AppEvent::PermissionRequest {
            request_id,
            tool_name,
            options,
            response_tx: resp_tx,
        });

        // 等待用户在弹窗中选择结果
        match resp_rx.await {
            Ok(option_id) => Ok(option_id),
            Err(_) => Ok(None), // channel 关闭，视为取消
        }
    }
}

//! gRPC 服务共享清理逻辑
//!
//! cancel_session 和 stop_agent 共享的清理步骤提取为独立函数，
//! 确保两个 RPC 方法的清理行为一致，避免遗漏。

use std::sync::Arc;

use agent_client_protocol::schema::StopReason;
use shared_types::{SessionNotify, SessionPromptEnd};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::model::AgentStatus;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE, push_session_update_with_project};
use crate::{CancelNotificationRequestWrapper, CancelResult};

pub async fn send_session_prompt_end(
    project_id: &str,
    session_id: &str,
    stop_reason: StopReason,
    error_message: Option<String>,
) {
    let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
        session_id: session_id.to_string(),
        stop_reason,
        error_message,
        request_id: None,
    });

    if let Err(e) = push_session_update_with_project(project_id, session_id, notify).await {
        warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
    } else {
        info!(
            "📤 [gRPC] SessionPromptEnd notification sent: session_id={}",
            session_id
        );
    }
}

pub async fn close_session_connection(session_id: &str) {
    if let Some(session_data_ref) = SESSION_CACHE.get(session_id) {
        let session_data = session_data_ref.clone();
        drop(session_data_ref);
        session_data.close_current_connection().await;
    }
}

pub fn remove_session_from_cache(session_id: &str) {
    if SESSION_CACHE.remove(session_id).is_some() {
        info!(
            "🗑️ [gRPC] SESSION_CACHE cleaned up: session_id={}",
            session_id
        );
    }
}

pub fn update_agent_status_to_idle(project_id: &str, status_filter: &[AgentStatus]) -> bool {
    use chrono::Utc;

    let updated = AGENT_REGISTRY.try_update_agent_info(project_id, |info| {
        if status_filter.contains(&info.status) {
            info.status = AgentStatus::Idle;
            info.last_activity = Utc::now();
            true
        } else {
            false
        }
    });

    if updated {
        info!(
            "✅ [gRPC] Agent status atomically updated to Idle: project_id={}",
            project_id
        );
    }
    updated
}

pub async fn cleanup_session_full(
    project_id: &str,
    session_id: &str,
    stop_reason: StopReason,
    error_message: Option<String>,
    update_status: bool,
    status_filter: &[AgentStatus],
) {
    send_session_prompt_end(project_id, session_id, stop_reason, error_message).await;
    close_session_connection(session_id).await;
    remove_session_from_cache(session_id);
    if update_status {
        update_agent_status_to_idle(project_id, status_filter);
    }
}

pub fn remove_agent_and_cleanup(project_id: &str) {
    let removed_agent_info = AGENT_REGISTRY.remove_by_project(project_id);

    if let Some(agent_info) = removed_agent_info {
        info!(
            "✅ [gRPC] Agent removed from Registry: project_id={}",
            project_id
        );

        if let Some(ref stop_handle) = agent_info.stop_handle {
            info!(
                "🔪 [gRPC] Force-stopping Agent child process: project_id={}",
                project_id
            );
            let stop_handle_clone = Arc::clone(stop_handle);
            let pid_clone = project_id.to_string();

            tokio::spawn(async move {
                if let Err(e) = stop_handle_clone.graceful_stop().await {
                    warn!(
                        "⚠️ [gRPC] graceful_stop failed: {}, project_id={}",
                        e, pid_clone
                    );
                } else {
                    info!(
                        "[gRPC] Agent child process stopped: project_id={}",
                        pid_clone
                    );
                }
                tokio::task::spawn_blocking(move || {
                    drop(agent_info);
                    info!(
                        "🧹 [gRPC] Agent resources fully cleaned up: project_id={}",
                        pid_clone
                    );
                });
            });
        } else {
            let pid_clone = project_id.to_string();
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    drop(agent_info);
                    info!(
                        "🧹 [gRPC] Agent resources fully cleaned up: project_id={}",
                        pid_clone
                    );
                });
            });
        }
    } else {
        warn!(
            "⚠️ [gRPC] Agent not in Registry: project_id={}",
            project_id
        );
    }
}

pub enum CancelAndWaitResult {
    Completed(CancelResult),
    SendFailed(String),
    ChannelClosed(String),
    Timeout,
}

pub async fn send_cancel_and_wait(
    cancel_tx: &mpsc::Sender<CancelNotificationRequestWrapper>,
    session_id: &str,
    timeout_secs: u64,
) -> CancelAndWaitResult {
    use agent_client_protocol::schema::{CancelNotification, SessionId};
    use tokio::sync::oneshot;
    use std::time::Duration;

    let session_id_obj = SessionId::new(Arc::from(session_id));
    let cancel_notification = CancelNotification::new(session_id_obj);

    let (result_tx, result_rx) = oneshot::channel::<CancelResult>();
    let cancel_request = CancelNotificationRequestWrapper {
        cancel_notification,
        result_tx,
    };

    if let Err(e) = cancel_tx.send(cancel_request).await {
        return CancelAndWaitResult::SendFailed(e.to_string());
    }

    info!(
        "📡 [gRPC] waiting for Agent cancel response: session_id={}",
        session_id
    );

    match tokio::time::timeout(Duration::from_secs(timeout_secs), result_rx).await {
        Ok(Ok(cancel_result)) => CancelAndWaitResult::Completed(cancel_result),
        Ok(Err(e)) => CancelAndWaitResult::ChannelClosed(e.to_string()),
        Err(_) => CancelAndWaitResult::Timeout,
    }
}

//! gRPC 服务共享清理逻辑
//!
//! cancel_session 和 stop_agent 共享的清理步骤提取为独立函数，
//! 确保两个 RPC 方法的清理行为一致，避免遗漏。

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use shared_types::{SessionNotify, SessionPromptEnd};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::model::AgentStatus;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE, SessionData};
use crate::{CancelNotificationRequestWrapper, CancelResult};

/// 控制请求只操作受理时捕获的 SSE 实例，await 后不按 sid 查找新缓存。
pub struct CapturedSessionCleanup {
    session_id: String,
    request_id: Option<String>,
    data: Option<Arc<SessionData>>,
    connection_ids: Vec<u64>,
}

impl CapturedSessionCleanup {
    pub fn capture(session_id: &str, request_id: Option<String>) -> Self {
        let data = SESSION_CACHE.view(session_id, |_, data| data.clone());
        let connection_ids = data
            .as_ref()
            .map(|data| data.capture_connection_ids())
            .unwrap_or_default();
        Self {
            session_id: session_id.to_string(),
            request_id,
            data,
            connection_ids,
        }
    }

    pub async fn send_prompt_end(
        &self,
        stop_reason: StopReason,
        error_message: Option<String>,
    ) -> shared_types::UnifiedSessionMessage {
        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
            session_id: self.session_id.clone(),
            stop_reason,
            error_message,
            request_id: self.request_id.clone(),
        });
        let terminal = notify.to_unified_message();
        if let Some(data) = &self.data
            && let Err(error) = data
                .push_captured_terminal(terminal.clone(), &self.connection_ids)
                .await
        {
            warn!(session_id = self.session_id, %error, "failed to notify captured SSE instance");
        }
        terminal
    }

    pub fn close(&self, terminal: &shared_types::UnifiedSessionMessage) {
        if let Some(data) = &self.data {
            data.close_captured_connections(&self.connection_ids, terminal);
        }
    }

    pub fn remove(&self) {
        if let Some(data) = &self.data {
            // cache 与本 cleanup 各持一个 Arc。Subscribe 在取缓存后保有独立 Arc，
            // 包括尚未注册的排队请求；有引用时保留路由，稍后由原闲置清理收回。
            SESSION_CACHE.remove_if(&self.session_id, |_, current| {
                Arc::ptr_eq(current, data)
                    && current.connections_len() == 0
                    && Arc::strong_count(current) == 2
            });
        }
    }
}

pub fn update_agent_status_to_idle(
    project_id: &str,
    session_id: &str,
    cancel_tx: &mpsc::Sender<CancelNotificationRequestWrapper>,
    request_id: Option<&str>,
    status_filter: &[AgentStatus],
) -> bool {
    use chrono::Utc;

    let updated = AGENT_REGISTRY.try_update_agent_info(project_id, |info| {
        if info.session_id.to_string() == session_id
            && info.cancel_tx.same_channel(cancel_tx)
            && info.request_id.as_deref() == request_id
            && status_filter.contains(&info.status)
        {
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
    cleanup: &CapturedSessionCleanup,
    stop_reason: StopReason,
    error_message: Option<String>,
    update_status: bool,
    status_filter: &[AgentStatus],
    cancel_tx: &mpsc::Sender<CancelNotificationRequestWrapper>,
) {
    let session_id = cleanup.session_id.as_str();
    let matches = {
        AGENT_REGISTRY
            .view_agent_info(project_id, |info| {
                info.session_id.to_string() == session_id && info.cancel_tx.same_channel(cancel_tx)
            })
            .unwrap_or(false)
    };
    if !matches {
        return;
    }
    let terminal = cleanup.send_prompt_end(stop_reason, error_message).await;
    cleanup.close(&terminal);
    // cancel 保持 agent 存活，也保持 SessionData/seq 和新订阅的后续消息路由。
    // Stop 在真实进程退出后单独执行 Arc-fenced remove。
    if update_status {
        update_agent_status_to_idle(
            project_id,
            session_id,
            cancel_tx,
            cleanup.request_id.as_deref(),
            status_filter,
        );
    }
}

pub async fn remove_agent_and_cleanup(
    project_id: &str,
    expected_session_id: &str,
) -> anyhow::Result<bool> {
    let target = AGENT_REGISTRY
        .view_agent_info(project_id, |info| {
            (info.session_id.to_string() == expected_session_id).then(|| info.clone())
        })
        .flatten();
    let Some(target) = target else {
        return Ok(false);
    };
    if let Some(handle) = &target.stop_handle {
        handle.graceful_stop().await?;
        handle.wait_for_exit().await?;
    } else if !target.prompt_tx.is_closed() {
        anyhow::bail!("agent lifecycle handle missing for live execution: {project_id}");
    }
    Ok(AGENT_REGISTRY
        .remove_by_project_if_session_and_channel_matches(
            project_id,
            expected_session_id,
            &target.cancel_tx,
        )
        .is_some())
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
    use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
    use std::time::Duration;
    use tokio::sync::oneshot;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captured_cleanup_cannot_close_or_remove_same_sid_new_cache_instance() {
        let sid = format!("captured-cleanup-{}", uuid::Uuid::new_v4());
        let old = SessionData::new(64).await;
        let (_old_id, _old_replay, mut old_rx, old_token) =
            old.create_new_connection(8, 0).await.unwrap();
        SESSION_CACHE.insert(sid.clone(), old);
        let cleanup = CapturedSessionCleanup::capture(&sid, Some("captured-old-request".into()));
        let replacement = SessionData::new(64).await;
        let (_id, _replay, mut replacement_rx, replacement_token) =
            replacement.create_new_connection(8, 0).await.unwrap();
        SESSION_CACHE.insert(sid.clone(), replacement.clone());
        let terminal = cleanup.send_prompt_end(StopReason::Cancelled, None).await;
        let (_seq, notification) = old_rx.recv().await.unwrap();
        assert!(matches!(
            notification.message_type,
            crate::model::SessionMessageType::SessionPromptEnd
        ));
        assert_eq!(notification.data["request_id"], "captured-old-request");
        cleanup.close(&terminal);
        cleanup.remove();
        assert!(old_token.is_cancelled());
        assert!(!replacement_token.is_cancelled());
        assert!(
            replacement_rx.try_recv().is_err(),
            "old terminal event leaked into replacement stream"
        );
        assert!(
            SESSION_CACHE
                .view(&sid, |_, current| Arc::ptr_eq(current, &replacement))
                .unwrap()
        );
        SESSION_CACHE.remove(&sid);
    }

    #[tokio::test]
    async fn captured_terminal_keeps_same_cache_new_connection_and_future_messages() {
        let sid = format!("captured-new-connection-{}", uuid::Uuid::new_v4());
        let data = SessionData::new(64).await;
        let old = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        SESSION_CACHE.insert(sid.clone(), data.clone());
        let cleanup =
            CapturedSessionCleanup::capture(&sid, Some("original-control-request".into()));
        let mut new = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        let terminal = cleanup.send_prompt_end(StopReason::Cancelled, None).await;
        cleanup.close(&terminal);
        cleanup.remove();
        assert!(old.token.is_cancelled());
        assert_eq!(
            old.terminal.borrow().as_ref().unwrap().data["request_id"],
            "original-control-request"
        );
        assert!(!new.token.is_cancelled());
        assert!(new.terminal.borrow().is_none());
        // 等 targeted Push 经过 worker，证明新连接连实际旧终态事件也未收到。
        let _ = data.message_count().await;
        assert!(new.rx.try_recv().is_err());
        assert!(
            SESSION_CACHE
                .view(&sid, |_, current| Arc::ptr_eq(current, &data))
                .unwrap()
        );
        crate::service::push_session_update_with_project(
            "captured-new-connection-project",
            &sid,
            SessionNotify::SessionPromptStart(shared_types::SessionPromptStart {
                session_id: sid.clone(),
                request_id: Some("new-request".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            new.rx.recv().await.unwrap().1.data["request_id"],
            "new-request"
        );
        SESSION_CACHE.remove(&sid);
    }

    #[tokio::test]
    async fn captured_remove_keeps_prefetched_subscribe_arc_before_registration() {
        let sid = format!("captured-prefetched-subscribe-{}", uuid::Uuid::new_v4());
        let data = SessionData::new(64).await;
        let old = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        SESSION_CACHE.insert(sid.clone(), data.clone());
        let cleanup = CapturedSessionCleanup::capture(&sid, Some("stopped-request".into()));
        // 实际 subscribe 的缓存读取/注册前窗口：独立 Arc 已取出，conn 尚未注册。
        let prefetched = SESSION_CACHE
            .view(&sid, |_, current| current.clone())
            .unwrap();
        drop(data);
        let terminal = cleanup.send_prompt_end(StopReason::Cancelled, None).await;
        cleanup.close(&terminal);
        assert!(old.token.is_cancelled());
        assert_eq!(prefetched.connections_len(), 0);
        cleanup.remove();
        assert!(
            SESSION_CACHE
                .view(&sid, |_, current| Arc::ptr_eq(current, &prefetched))
                .unwrap()
        );
        let new = prefetched
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        assert!(!new.token.is_cancelled());
        assert!(new.terminal.borrow().is_none());
        SESSION_CACHE.remove(&sid);
    }
}

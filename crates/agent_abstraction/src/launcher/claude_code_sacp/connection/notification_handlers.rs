//! SACP 通知/消息类型处理
//!
//! 从 `run_sacp_connection` 抽出的 SessionNotification dispatch 处理器
//! 与会话更新通知分发逻辑。

use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::{Dispatch, Handled, JsonRpcMessage};
use tracing::{debug, error, warn};

use crate::traits::session_notifier::SessionNotifier;

/// 处理入站 Dispatch（目前仅处理 SessionNotification，其余交还 SACP 默认处理）
pub(super) async fn handle_incoming_dispatch<N: SessionNotifier>(
    dispatch: Dispatch,
    notifier: Arc<N>,
    project_id: String,
) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
    match dispatch {
        Dispatch::Notification(message) => {
            if SessionNotification::matches_method(&message.method) {
                match SessionNotification::parse_message(&message.method, &message.params) {
                    Ok(notification) => {
                        // notifier/project_id 按值持有且此后不再使用，直接 move，避免额外 clone
                        handle_session_notification(notification, notifier, project_id).await;
                        Ok(Handled::Yes)
                    }
                    Err(err) => {
                        // 🔥 关键：未知消息类型只打 warn，不中断连接
                        warn!(
                            "[SACP] Failed to parse SessionNotification, ignoring: method={}, error={:?}, json={:?}",
                            message.method, err, message.params
                        );
                        Ok(Handled::Yes)
                    }
                }
            } else {
                Ok(Handled::No {
                    message: Dispatch::Notification(message),
                    retry: false,
                })
            }
        }
        other => Ok(Handled::No {
            message: other,
            retry: false,
        }),
    }
}

/// 处理 SessionNotification 回调
async fn handle_session_notification<N: SessionNotifier>(
    notification: SessionNotification,
    notifier: Arc<N>,
    project_id: String,
) {
    let session_id = notification.session_id.to_string();

    debug!(
        "[SACP] SessionNotification: project_id={}, session_id={}, update={:?}",
        project_id, session_id, notification.update
    );

    // 提取 request_id（如果有）
    let request_id = notification
        .meta
        .as_ref()
        .and_then(|meta| meta.get("request_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 通过 notifier 推送会话更新
    // SessionUpdate 通过 agent_client_protocol::schema 导入
    if let Err(e) = notifier
        .notify_session_update(&project_id, &session_id, notification.update, request_id)
        .await
    {
        error!(
            "[SACP] Push session update failed: project_id={}, session_id={}, error={:?}",
            project_id, session_id, e
        );
    }
}

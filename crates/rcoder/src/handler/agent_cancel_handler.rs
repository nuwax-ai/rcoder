//! Agent任务取消处理器
//!
//! 通过ACP协议的CancelNotification来取消指定session的agent任务执行

use axum::{
    extract::{Path, Query},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info};

use agent_client_protocol::{CancelNotification, SessionId};

use crate::{AppError, HttpResult};
use crate::{CancelNotificationRequest, proxy_agent::PROJECT_AND_AGENT_INFO_MAP};

/// 取消任务的查询参数
#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    /// 项目ID
    pub project_id: String,
    /// 会话ID
    pub session_id: String,
}

/// 取消任务的响应
#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub success: bool,
    pub session_id: String,
}

/// 处理agent任务取消请求
///
/// 通过ACP协议的CancelNotification取消指定session的agent任务
pub async fn agent_session_cancel(
    Query(query): Query<CancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    info!(
        "🛑 收到取消任务请求: session_id={}, project_id={:?}",
        query.session_id, query.project_id
    );
    let session_id = query.session_id;
    let project_id = query.project_id;

    // 创建SessionId
    let session_id_obj = SessionId(Arc::from(session_id.as_str()));

    // 创建CancelNotification
    let cancel_notification = CancelNotification {
        session_id: session_id_obj,
        meta: None,
    };

    // 从全局映射中查找匹配的session
    let project_info = PROJECT_AND_AGENT_INFO_MAP.get(&project_id);

    let (tx, rx) = oneshot::channel();
    let cancel_notification_request = CancelNotificationRequest {
        cancel_notification,
        tx,
    };
    match project_info {
        Some(project_info) => {
            debug!("🔍 查找session: {} 对应的agent连接", session_id);

            // 通过cancel_tx发送取消通知
            project_info
                .cancel_tx
                .send(cancel_notification_request)
                .map_err(|e| anyhow::anyhow!("发送取消通知失败: {}", e))?;

            // 等待取消通知响应
            match rx.await {
                Ok(cancel_notification_response) => {
                    if cancel_notification_response.success {
                        Ok(HttpResult::success(CancelResponse {
                            success: true,
                            session_id: session_id,
                        }))
                    } else {
                        Ok(HttpResult::error("0001", "停止智能体执行失败"))
                    }
                }
                Err(e) => Err(AppError::AnyhowError(anyhow::anyhow!(
                    "停止智能体执行失败: {}",
                    e
                ))),
            }
        }
        None => {
            error!("❌ 未找到project_id: {} 对应的活跃连接", project_id);

            Ok(HttpResult::error("0001", "未找到project_id对应的活跃连接"))
        }
    }
}

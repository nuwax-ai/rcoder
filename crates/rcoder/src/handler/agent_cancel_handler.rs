//! Agent任务取消处理器
//!
//! 通过ACP协议的CancelNotification来取消指定session的agent任务执行

use axum::{
    extract::{Path, Query},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

use agent_client_protocol::{CancelNotification, SessionId};

use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;
use crate::{AppError, HttpResult};

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
    pub message: String,
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

    // 通过全局PROJECT_AND_AGENT_INFO_MAP获取对应session的取消通道
    match try_send_cancel_notification(&session_id, &project_id, cancel_notification).await {
        Ok(_) => {
            info!("✅ 成功发送取消通知: session_id={}", session_id);

            let response = CancelResponse {
                success: true,
                message: "取消通知已发送".to_string(),
                session_id: session_id.clone(),
            };

            Ok(HttpResult::success(response))
        }
        Err(e) => {
            error!(
                "❌ 发送取消通知失败: session_id={}, error={}",
                session_id, e
            );

            let response = CancelResponse {
                success: false,
                message: format!("发送取消通知失败: {}", e),
                session_id: session_id.clone(),
            };

            Ok(HttpResult::success(response))
        }
    }
}

/// 尝试发送取消通知
async fn try_send_cancel_notification(
    session_id: &str,
    project_id: &str,
    cancel_notification: CancelNotification,
) -> Result<(), String> {
    // 从全局映射中查找匹配的session
    let project_info = PROJECT_AND_AGENT_INFO_MAP.get(project_id);

    match project_info {
        Some(project_info) => {
            info!("🔍 查找session: {} 对应的agent连接", session_id);

            // 通过cancel_tx发送取消通知
            match project_info.cancel_tx.send(cancel_notification) {
                Ok(_) => {
                    info!("📤 取消通知成功发送到session: {}", session_id);
                    return Ok(());
                }
                Err(e) => {
                    error!("❌ 发送取消通知失败: session={}, error={}", session_id, e);
                    return Err(format!("发送取消通知失败: {}", e));
                }
            }
        }
        None => {
            error!("❌ 未找到project_id: {} 对应的活跃连接", project_id);

            Err(format!("未找到session_id: {} 对应的活跃连接", session_id))
        }
    }
}

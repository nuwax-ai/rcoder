//! Agent任务取消处理器
//!
//! 通过ACP协议的CancelNotification来取消指定session的agent任务执行

use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use utoipa::{IntoParams, ToSchema};

use agent_client_protocol::{CancelNotification, SessionId};

use crate::{
    CancelNotificationRequest, proxy_agent::PROJECT_AND_AGENT_INFO_MAP,
};
use crate::{model::AppError, model::HttpResult, router::AppState};

/// 取消任务的查询参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct CancelQuery {
    /// 项目ID，用于标识特定的项目
    #[param(example = "test_project")]
    pub project_id: String,
    /// 会话ID，用于标识要取消的会话
    #[param(example = "session456")]
    pub session_id: String,
}

/// 取消任务的响应
#[derive(Debug, Serialize, ToSchema)]
pub struct CancelResponse {
    /// 取消操作是否成功
    #[schema(example = true)]
    pub success: bool,
    /// 被取消的会话ID
    #[schema(example = "session456")]
    pub session_id: String,
}

/// 处理agent任务取消请求
///
/// 通过ACP协议的CancelNotification取消指定session的agent任务执行
#[utoipa::path(
    post,
    path = "/agent/session/cancel",
    params(
        CancelQuery
    ),
    responses(
        (
            status = 200,
            description = "成功发送取消请求",
            body = HttpResult<CancelResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "session_id": "session456"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INVALID_PARAMS",
                    "message": "Invalid project_id or session_id"
                }
            })
        ),
        (
            status = 404,
            description = "未找到对应的项目或会话",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "PROJECT_NOT_FOUND",
                    "message": "Project or session not found"
                }
            })
        ),
        (
            status = 500,
            description = "取消操作失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CANCEL_FAILED",
                    "message": "Failed to cancel agent task"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_cancel",
    summary = "取消Agent任务",
    description = "通过ACP协议发送取消通知，停止指定会话的AI代理任务执行"
)]
pub async fn agent_session_cancel(
    State(_state): State<Arc<AppState>>,
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

            // 🎯 先设置session取消标记，确保后续Agent消息被过滤
            if let Some(session_data) = crate::service::SESSION_CACHE.get(&session_id) {
                session_data.set_cancelled(true);
                info!("🚫 已设置session取消标记: session_id={}", session_id);
            }

            // 🔄 修复竞争条件：先发送取消通知，等待Agent完成取消后再清空缓存
            // 通过cancel_tx发送取消通知
            project_info
                .cancel_tx
                .send(cancel_notification_request)
                .map_err(|e| anyhow::anyhow!("发送取消通知失败: {}", e))?;

            // 等待取消通知响应
            info!("📡 [agent_cancel_handler] 等待Agent取消响应: session_id={}", session_id);
            match rx.await {
                Ok(cancel_notification_response) => {
                    info!("✅ [agent_cancel_handler] 收到Agent取消响应: session_id={}, success={}", session_id, cancel_notification_response.success);
                    if cancel_notification_response.success {
                        // 🧹 彻底清空该 session，避免阻塞
                        // 直接移除 SESSION_CACHE 条目，绕过 SessionWorker 队列
                        if crate::service::SESSION_CACHE.remove(&session_id).is_some() {
                            info!("🗑️ Agent取消成功，直接移除 SESSION_CACHE 条目: session_id={}", session_id);
                        }

                        Ok(HttpResult::success(CancelResponse {
                            success: true,
                            session_id,
                        }))
                    } else {
                        // 如果取消失败，也要重置取消标记，让用户可以重试
                        if let Some(session_data) = crate::service::SESSION_CACHE.get(&session_id) {
                            session_data.set_cancelled(false);
                            info!("🔄 取消失败，已重置session取消标记: session_id={}, error={:?}", session_id, cancel_notification_response.message);
                        }
                        Ok(HttpResult::error("0001", "停止智能体执行失败"))
                    }
                }
                Err(e) => {
                    error!("❌ [agent_cancel_handler] 等待Agent取消响应失败: session_id={}, error={:?}", session_id, e);
                    // 如果取消过程出错，也要重置取消标记
                    if let Some(session_data) = crate::service::SESSION_CACHE.get(&session_id) {
                        session_data.set_cancelled(false);
                        info!("🔄 取消过程出错，已重置session取消标记: session_id={}", session_id);
                    }
                    Err(AppError::AnyhowError(anyhow::anyhow!(
                        "停止智能体执行失败: {}",
                        e
                    )))
                }
            }
        }
        None => {
            warn!(
                "❌ 未找到project_id: {} 对应的活跃连接,无需取消agent当前任务",
                project_id
            );

            // 🎯 即使没有找到活跃连接，也要设置取消标记，防止可能的残留消息
            if let Some(session_data) = crate::service::SESSION_CACHE.get(&session_id) {
                session_data.set_cancelled(true);
                info!("🚫 [agent_cancel] 未找到活跃连接，仍设置session取消标记: session_id={}, project_id={}", session_id, project_id);
            }

            Ok(HttpResult::success(CancelResponse {
                success: true,
                session_id,
            }))
        }
    }
}

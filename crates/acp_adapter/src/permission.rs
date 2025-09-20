//! 工具调用权限管理模块
//!
//! 实现类似 Zed 的工具调用权限确认机制，包括：
//! - 权限请求的创建和管理
//! - 用户确认流程的处理
//! - 权限选项和响应的管理

use crate::types::{
    ExtendedToolCallStatus, PermissionRequest, PermissionResponse, PermissionOutcome,
    ToolCallId, PermissionRequestState, SerializableToolCallStatus
};
use agent_client_protocol::{PermissionOption, PermissionOptionId, PermissionOptionKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn, error};
use uuid::Uuid;

/// 权限管理器 - 处理工具调用的权限确认
#[derive(Debug)]
pub struct PermissionManager {
    /// 待处理的权限请求
    pending_requests: Arc<RwLock<HashMap<ToolCallId, PendingPermissionRequest>>>,
    /// 权限设置
    settings: Arc<RwLock<PermissionSettings>>,
    /// 事件发送通道
    event_sender: mpsc::UnboundedSender<PermissionEvent>,
}

/// 待处理的权限请求
#[derive(Debug)]
struct PendingPermissionRequest {
    state: PermissionRequestState,
    respond_tx: oneshot::Sender<PermissionOutcome>,
}

/// 权限设置
#[derive(Debug, Clone)]
pub struct PermissionSettings {
    /// 总是允许工具操作（类似 Zed 的 agent.always_allow_tool_actions）
    pub always_allow_tool_actions: bool,
    /// 自动允许的工具类型
    pub auto_allow_tools: Vec<String>,
    /// 自动拒绝的工具类型
    pub auto_deny_tools: Vec<String>,
    /// 权限请求超时时间（秒）
    pub request_timeout_seconds: u64,
    /// 默认权限选项
    pub default_options: Vec<PermissionOption>,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            always_allow_tool_actions: false,
            auto_allow_tools: vec!["read_file".to_string(), "thinking".to_string()],
            auto_deny_tools: vec![],
            request_timeout_seconds: 300, // 5 分钟
            default_options: vec![
                PermissionOption {
                    id: PermissionOptionId("allow_once".into()),
                    kind: PermissionOptionKind::AllowOnce,
                    name: "Allow once".into(),
                    meta: None,
                },
                PermissionOption {
                    id: PermissionOptionId("allow_always".into()),
                    kind: PermissionOptionKind::AllowAlways,
                    name: "Always allow".into(),
                    meta: None,
                },
                PermissionOption {
                    id: PermissionOptionId("reject_once".into()),
                    kind: PermissionOptionKind::RejectOnce,
                    name: "Reject once".into(),
                    meta: None,
                },
                PermissionOption {
                    id: PermissionOptionId("reject_always".into()),
                    kind: PermissionOptionKind::RejectAlways,
                    name: "Never allow".into(),
                    meta: None,
                },
            ],
        }
    }
}

/// 权限事件
#[derive(Debug, Clone)]
pub enum PermissionEvent {
    /// 权限请求创建
    RequestCreated {
        tool_call_id: ToolCallId,
        tool_name: String,
        description: String,
        options: Vec<PermissionOption>,
    },
    /// 权限响应接收
    ResponseReceived {
        tool_call_id: ToolCallId,
        outcome: PermissionOutcome,
    },
    /// 权限请求超时
    RequestTimeout {
        tool_call_id: ToolCallId,
    },
    /// 权限设置更新
    SettingsUpdated {
        settings: PermissionSettings,
    },
}

impl PermissionManager {
    /// 创建新的权限管理器
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PermissionEvent>) {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        
        let manager = Self {
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
            settings: Arc::new(RwLock::new(PermissionSettings::default())),
            event_sender,
        };
        
        (manager, event_receiver)
    }

    /// 更新权限设置
    pub async fn update_settings(&self, settings: PermissionSettings) {
        let mut current_settings = self.settings.write().await;
        *current_settings = settings.clone();
        
        if let Err(e) = self.event_sender.send(PermissionEvent::SettingsUpdated { settings }) {
            error!("发送权限设置更新事件失败: {}", e);
        }
        
        info!("权限设置已更新");
    }

    /// 获取当前权限设置
    pub async fn get_settings(&self) -> PermissionSettings {
        self.settings.read().await.clone()
    }

    /// 请求权限 - 类似 Zed 的 request_permission 方法
    pub async fn request_permission(
        &self,
        request: PermissionRequest,
    ) -> Result<PermissionOutcome, String> {
        let settings = self.settings.read().await;
        
        // 检查是否总是允许工具操作
        if settings.always_allow_tool_actions {
            debug!("总是允许工具操作模式，自动允许工具调用: {}", request.tool_name);
            return Ok(PermissionOutcome::Selected {
                option_id: PermissionOptionId("allow_once".into()),
            });
        }

        // 检查自动允许列表
        if settings.auto_allow_tools.contains(&request.tool_name) {
            debug!("工具 {} 在自动允许列表中，自动允许", request.tool_name);
            return Ok(PermissionOutcome::Selected {
                option_id: PermissionOptionId("allow_once".into()),
            });
        }

        // 检查自动拒绝列表
        if settings.auto_deny_tools.contains(&request.tool_name) {
            debug!("工具 {} 在自动拒绝列表中，自动拒绝", request.tool_name);
            return Ok(PermissionOutcome::Selected {
                option_id: PermissionOptionId("reject_once".into()),
            });
        }

        // 创建权限请求状态
        let expires_at = Some(SystemTime::now() + Duration::from_secs(settings.request_timeout_seconds));
        let state = PermissionRequestState {
            tool_call_id: request.tool_call_id.clone(),
            options: settings.default_options.clone(),
            created_at: SystemTime::now(),
            expires_at,
        };

        // 创建响应通道
        let (respond_tx, respond_rx) = oneshot::channel();

        // 存储待处理请求
        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(
                request.tool_call_id.clone(),
                PendingPermissionRequest {
                    state: state.clone(),
                    respond_tx,
                },
            );
        }

        // 发送权限请求事件
        if let Err(e) = self.event_sender.send(PermissionEvent::RequestCreated {
            tool_call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
            description: request.description.clone(),
            options: settings.default_options.clone(),
        }) {
            error!("发送权限请求创建事件失败: {}", e);
        }

        drop(settings); // 释放读锁

        info!("已创建权限请求: tool_call_id={}, tool_name={}", request.tool_call_id, request.tool_name);

        // 设置超时处理
        let tool_call_id_for_timeout = request.tool_call_id.clone();
        let pending_requests_for_timeout = self.pending_requests.clone();
        let event_sender_for_timeout = self.event_sender.clone();
        
        tokio::spawn(async move {
            if let Some(expires_at) = expires_at {
                let timeout_duration = expires_at.duration_since(SystemTime::now())
                    .unwrap_or(Duration::from_secs(0));
                
                tokio::time::sleep(timeout_duration).await;
                
                // 检查请求是否仍然待处理
                let mut pending = pending_requests_for_timeout.write().await;
                if pending.remove(&tool_call_id_for_timeout).is_some() {
                    warn!("权限请求超时: tool_call_id={}", tool_call_id_for_timeout);
                    
                    if let Err(e) = event_sender_for_timeout.send(PermissionEvent::RequestTimeout {
                        tool_call_id: tool_call_id_for_timeout,
                    }) {
                        error!("发送权限请求超时事件失败: {}", e);
                    }
                }
            }
        });

        // 等待响应
        match respond_rx.await {
            Ok(outcome) => {
                info!("权限请求已响应: tool_call_id={}, outcome={:?}", request.tool_call_id, outcome);
                Ok(outcome)
            }
            Err(_) => {
                warn!("权限请求通道已关闭: tool_call_id={}", request.tool_call_id);
                Ok(PermissionOutcome::Cancelled)
            }
        }
    }

    /// 响应权限请求 - 类似 Zed 的 authorize_tool_call 方法
    pub async fn respond_to_permission(
        &self,
        tool_call_id: ToolCallId,
        option_id: PermissionOptionId,
        option_kind: PermissionOptionKind,
    ) -> Result<(), String> {
        let mut pending = self.pending_requests.write().await;
        
        if let Some(pending_request) = pending.remove(&tool_call_id) {
            let outcome = match option_kind {
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => {
                    PermissionOutcome::Selected { option_id }
                }
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
                    PermissionOutcome::Selected { option_id }
                }
            };

            // 发送响应
            if let Err(_) = pending_request.respond_tx.send(outcome.clone()) {
                warn!("发送权限响应失败: tool_call_id={}", tool_call_id);
            }

            // 发送事件
            if let Err(e) = self.event_sender.send(PermissionEvent::ResponseReceived {
                tool_call_id: tool_call_id.clone(),
                outcome,
            }) {
                error!("发送权限响应事件失败: {}", e);
            }

            // 更新自动规则（如果选择了 Always 选项）
            if matches!(option_kind, PermissionOptionKind::AllowAlways | PermissionOptionKind::RejectAlways) {
                // TODO: 实现自动规则更新逻辑
                info!("需要更新自动规则: tool_call_id={}, option_kind={:?}", tool_call_id, option_kind);
            }

            info!("权限请求已处理: tool_call_id={}, option_kind={:?}", tool_call_id, option_kind);
            Ok(())
        } else {
            let error_msg = format!("未找到权限请求: tool_call_id={}", tool_call_id);
            warn!("{}", error_msg);
            Err(error_msg)
        }
    }

    /// 取消权限请求
    pub async fn cancel_permission_request(&self, tool_call_id: &ToolCallId) -> Result<(), String> {
        let mut pending = self.pending_requests.write().await;
        
        if let Some(pending_request) = pending.remove(tool_call_id) {
            if let Err(_) = pending_request.respond_tx.send(PermissionOutcome::Cancelled) {
                warn!("发送权限取消响应失败: tool_call_id={}", tool_call_id);
            }

            info!("权限请求已取消: tool_call_id={}", tool_call_id);
            Ok(())
        } else {
            let error_msg = format!("未找到权限请求: tool_call_id={}", tool_call_id);
            warn!("{}", error_msg);
            Err(error_msg)
        }
    }

    /// 获取待处理的权限请求列表
    pub async fn get_pending_requests(&self) -> Vec<PermissionRequestState> {
        let pending = self.pending_requests.read().await;
        pending.values().map(|req| req.state.clone()).collect()
    }

    /// 清理过期的权限请求
    pub async fn cleanup_expired_requests(&self) {
        let now = SystemTime::now();
        let mut pending = self.pending_requests.write().await;
        let mut expired_ids = Vec::new();

        for (tool_call_id, request) in pending.iter() {
            if let Some(expires_at) = request.state.expires_at {
                if now > expires_at {
                    expired_ids.push(tool_call_id.clone());
                }
            }
        }

        for tool_call_id in expired_ids {
            if let Some(pending_request) = pending.remove(&tool_call_id) {
                if let Err(_) = pending_request.respond_tx.send(PermissionOutcome::Expired) {
                    warn!("发送权限过期响应失败: tool_call_id={}", tool_call_id);
                }
                info!("已清理过期权限请求: tool_call_id={}", tool_call_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_always_allow_tool_actions() {
        let (manager, mut events) = PermissionManager::new();
        
        // 设置总是允许
        let mut settings = PermissionSettings::default();
        settings.always_allow_tool_actions = true;
        manager.update_settings(settings).await;

        let request = PermissionRequest {
            tool_call_id: ToolCallId::new(),
            tool_name: "test_tool".to_string(),
            description: "Test tool".to_string(),
            arguments: serde_json::json!({}),
        };

        let outcome = manager.request_permission(request).await.unwrap();
        
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
        
        // 应该收到设置更新事件，但不应该收到权限请求事件
        let event = events.recv().await.unwrap();
        assert!(matches!(event, PermissionEvent::SettingsUpdated { .. }));
    }

    #[tokio::test]
    async fn test_auto_allow_tools() {
        let (manager, mut events) = PermissionManager::new();
        
        let request = PermissionRequest {
            tool_call_id: ToolCallId::new(),
            tool_name: "read_file".to_string(), // 在默认自动允许列表中
            description: "Read file".to_string(),
            arguments: serde_json::json!({}),
        };

        let outcome = manager.request_permission(request).await.unwrap();
        
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
        
        // 不应该收到权限请求事件（因为自动允许）
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_permission_request_and_response() {
        let (manager, mut events) = PermissionManager::new();
        
        let tool_call_id = ToolCallId::new();
        let request = PermissionRequest {
            tool_call_id: tool_call_id.clone(),
            tool_name: "execute_command".to_string(),
            description: "Execute shell command".to_string(),
            arguments: serde_json::json!({"command": "ls -la"}),
        };

        // 在后台处理权限请求
        let pending_requests_for_response = manager.pending_requests.clone();
        let event_sender_for_response = manager.event_sender.clone();
        let tool_call_id_clone = tool_call_id.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            let mut pending = pending_requests_for_response.write().await;
            if let Some(pending_request) = pending.remove(&tool_call_id_clone) {
                let outcome = PermissionOutcome::Selected {
                    option_id: PermissionOptionId("allow_once".into()),
                };
                if let Err(_) = pending_request.respond_tx.send(outcome.clone()) {
                    warn!("发送权限响应失败: tool_call_id={}", tool_call_id_clone);
                }
                if let Err(e) = event_sender_for_response.send(PermissionEvent::ResponseReceived {
                    tool_call_id: tool_call_id_clone,
                    outcome,
                }) {
                    error!("发送权限响应事件失败: {}", e);
                }
            }
        });

        let outcome = manager.request_permission(request).await.unwrap();
        
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
        
        // 应该收到权限请求创建事件
        let event = events.recv().await.unwrap();
        assert!(matches!(event, PermissionEvent::RequestCreated { .. }));
        
        // 应该收到权限响应事件
        let event = events.recv().await.unwrap();
        assert!(matches!(event, PermissionEvent::ResponseReceived { .. }));
    }
}
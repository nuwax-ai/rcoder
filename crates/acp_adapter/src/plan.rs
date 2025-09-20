use crate::types::{Plan, PlanEntry, PlanEntryStatus, PlanEntryPriority, PlanStats, StreamUpdate};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn, error, debug};

/// Plan管理器 - 负责管理Plan的生命周期和ACP协议集成
#[derive(Debug)]
pub struct PlanManager {
    /// 当前活跃的Plans，按session_id索引
    plans: Arc<RwLock<HashMap<String, Plan>>>,
    /// 事件发送器
    event_tx: mpsc::UnboundedSender<PlanEvent>,
    /// 用于通知前端的更新通道
    update_senders: Arc<RwLock<Vec<mpsc::UnboundedSender<PlanUpdateEvent>>>>,
}

/// Plan事件
#[derive(Debug, Clone)]
pub enum PlanEvent {
    /// Plan创建
    Created {
        session_id: String,
        plan: Plan,
    },
    /// Plan更新
    Updated {
        session_id: String,
        plan: Plan,
    },
    /// 条目状态更新
    EntryStatusUpdated {
        session_id: String,
        entry_id: String,
        old_status: PlanEntryStatus,
        new_status: PlanEntryStatus,
    },
    /// Plan删除
    Deleted {
        session_id: String,
    },
}

/// 前端Plan更新事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdateEvent {
    /// 会话ID
    pub session_id: String,
    /// 更新类型
    pub update_type: PlanUpdateType,
    /// Plan数据
    pub plan: Option<Plan>,
    /// 统计信息
    pub stats: Option<PlanStats>,
    /// 时间戳
    pub timestamp: u64,
}

/// Plan更新类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlanUpdateType {
    /// 完整Plan更新
    FullUpdate,
    /// 条目状态更新
    EntryStatusUpdate {
        entry_id: String,
        status: PlanEntryStatus,
    },
    /// 新条目添加
    EntryAdded {
        entry_id: String,
    },
    /// 条目删除
    EntryRemoved {
        entry_id: String,
    },
    /// 统计信息更新
    StatsUpdate,
}

impl PlanManager {
    /// 创建新的Plan管理器
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PlanEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        
        let manager = Self {
            plans: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            update_senders: Arc::new(RwLock::new(Vec::new())),
        };
        
        (manager, event_rx)
    }
    
    /// 订阅Plan更新事件（用于前端）
    pub async fn subscribe_updates(&self) -> mpsc::UnboundedReceiver<PlanUpdateEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.update_senders.write().await.push(tx);
        rx
    }
    
    /// 创建或更新Plan
    pub async fn update_plan(&self, session_id: &str, plan: Plan) -> Result<()> {
        let is_new = {
            let plans = self.plans.read().await;
            !plans.contains_key(session_id)
        };
        
        // 存储Plan
        {
            let mut plans = self.plans.write().await;
            plans.insert(session_id.to_string(), plan.clone());
        }
        
        // 发送事件
        let event = if is_new {
            PlanEvent::Created {
                session_id: session_id.to_string(),
                plan: plan.clone(),
            }
        } else {
            PlanEvent::Updated {
                session_id: session_id.to_string(),
                plan: plan.clone(),
            }
        };
        
        self.event_tx.send(event).map_err(|e| anyhow::anyhow!("Failed to send plan event: {}", e))?;
        
        // 通知前端
        self.notify_frontend_update(session_id, PlanUpdateType::FullUpdate, Some(plan)).await;
        
        info!("Plan updated for session: {}", session_id);
        Ok(())
    }
    
    /// 更新Plan条目状态
    pub async fn update_entry_status(
        &self, 
        session_id: &str, 
        entry_id: &str, 
        status: PlanEntryStatus
    ) -> Result<()> {
        let (old_status, plan_clone) = {
            let mut plans = self.plans.write().await;
            
            if let Some(plan) = plans.get_mut(session_id) {
                if let Some(entry) = plan.entries.iter_mut().find(|e| e.id == entry_id) {
                    let old_status = entry.status.clone();
                    entry.status = status.clone();
                    entry.updated_at = std::time::SystemTime::now();
                    plan.updated_at = entry.updated_at;
                    
                    (old_status, plan.clone())
                } else {
                    return Err(anyhow::anyhow!("Entry {} not found in session {}", entry_id, session_id));
                }
            } else {
                return Err(anyhow::anyhow!("Plan not found for session: {}", session_id));
            }
        };
        
        // 发送状态更新事件
        let event = PlanEvent::EntryStatusUpdated {
            session_id: session_id.to_string(),
            entry_id: entry_id.to_string(),
            old_status,
            new_status: status.clone(),
        };
        
        self.event_tx.send(event).map_err(|e| anyhow::anyhow!("Failed to send entry status event: {}", e))?;
        
        // 通知前端
        self.notify_frontend_update(
            session_id, 
            PlanUpdateType::EntryStatusUpdate {
                entry_id: entry_id.to_string(),
                status,
            },
            Some(plan_clone)
        ).await;
        
        info!("Updated entry {} status for session: {}", entry_id, session_id);
        Ok(())
    }
    
    /// 获取Plan
    pub async fn get_plan(&self, session_id: &str) -> Option<Plan> {
        let plans = self.plans.read().await;
        plans.get(session_id).cloned()
    }
    
    /// 获取Plan统计信息
    pub async fn get_plan_stats(&self, session_id: &str) -> Option<PlanStats> {
        let plans = self.plans.read().await;
        plans.get(session_id).map(|plan| plan.stats())
    }
    
    /// 删除Plan
    pub async fn delete_plan(&self, session_id: &str) -> Result<()> {
        let mut plans = self.plans.write().await;
        
        if plans.remove(session_id).is_some() {
            let event = PlanEvent::Deleted {
                session_id: session_id.to_string(),
            };
            
            self.event_tx.send(event).map_err(|e| anyhow::anyhow!("Failed to send plan delete event: {}", e))?;
            
            info!("Plan deleted for session: {}", session_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Plan not found for session: {}", session_id))
        }
    }
    
    /// 列出所有活跃的Plan
    pub async fn list_active_plans(&self) -> HashMap<String, PlanStats> {
        let plans = self.plans.read().await;
        plans.iter()
            .map(|(session_id, plan)| (session_id.clone(), plan.stats()))
            .collect()
    }
    
    /// 清理已完成的Plan条目
    pub async fn cleanup_completed_entries(&self, session_id: &str) -> Result<()> {
        let mut plans = self.plans.write().await;
        
        if let Some(plan) = plans.get_mut(session_id) {
            let before_count = plan.entries.len();
            plan.clear_completed();
            let after_count = plan.entries.len();
            
            if before_count != after_count {
                info!("Cleaned up {} completed entries for session: {}", before_count - after_count, session_id);
                
                // 通知前端统计信息更新
                self.notify_frontend_update(
                    session_id, 
                    PlanUpdateType::StatsUpdate,
                    Some(plan.clone())
                ).await;
            }
            
            Ok(())
        } else {
            Err(anyhow::anyhow!("Plan not found for session: {}", session_id))
        }
    }
    
    /// 通知前端更新
    async fn notify_frontend_update(&self, session_id: &str, update_type: PlanUpdateType, plan: Option<Plan>) {
        let update_event = PlanUpdateEvent {
            session_id: session_id.to_string(),
            update_type,
            stats: plan.as_ref().map(|p| p.stats()),
            plan,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        
        let senders = self.update_senders.read().await;
        let mut failed_indices = Vec::new();
        
        for (index, sender) in senders.iter().enumerate() {
            if let Err(_) = sender.send(update_event.clone()) {
                failed_indices.push(index);
            }
        }
        
        // 清理失败的发送者
        if !failed_indices.is_empty() {
            drop(senders);
            let mut senders = self.update_senders.write().await;
            
            // 从后往前删除，避免索引偏移
            for &index in failed_indices.iter().rev() {
                if index < senders.len() {
                    senders.remove(index);
                }
            }
        }
    }
}

impl Default for PlanManager {
    fn default() -> Self {
        Self::new().0
    }
}

/// Plan与ACP协议的转换工具
pub struct PlanConverter;

impl PlanConverter {
    /// 将ACP Plan转换为内部Plan结构
    pub fn from_acp_plan(acp_plan: agent_client_protocol::Plan) -> Plan {
        let now = std::time::SystemTime::now();
        
        Plan {
            entries: acp_plan.entries.into_iter().map(Self::from_acp_entry).collect(),
            created_at: now,
            updated_at: now,
            title: None,
            description: None,
            category: None,
            total_estimated_duration: None,
            total_actual_duration: None,
            status: crate::types::PlanStatus::NotStarted,
            meta: None,
        }
    }
    
    /// 将ACP PlanEntry转换为内部PlanEntry结构
    fn from_acp_entry(acp_entry: agent_client_protocol::PlanEntry) -> PlanEntry {
        let now = std::time::SystemTime::now();
        
        PlanEntry {
            id: uuid::Uuid::new_v4().to_string(), // ACP可能没有ID，我们生成一个
            content: acp_entry.content,
            priority: PlanEntryPriority::Normal, // 默认优先级
            status: PlanEntryStatus::Pending, // 默认状态
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            estimated_duration: None,
            actual_duration: None,
            tags: Vec::new(),
            description: None,
            dependencies: Vec::new(),
            progress: Some(0),
            meta: acp_entry.meta,
        }
    }
    
    /// 将内部Plan转换为StreamUpdate
    pub fn to_stream_update(session_id: &str, plan: &Plan) -> StreamUpdate {
        StreamUpdate::Plan {
            session_id: agent_client_protocol::SessionId(session_id.into()),
            plan: serde_json::to_value(plan).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};
    
    #[tokio::test]
    async fn test_plan_manager_basic_operations() {
        let (manager, mut event_rx) = PlanManager::new();
        let session_id = "test_session";
        
        // 创建Plan
        let mut plan = Plan::new();
        let entry_id = plan.add_entry("Test task".to_string(), PlanEntryPriority::Normal);
        
        // 更新Plan
        manager.update_plan(session_id, plan.clone()).await.unwrap();
        
        // 验证事件
        let event = timeout(Duration::from_millis(100), event_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(event, PlanEvent::Created { .. }));
        
        // 获取Plan
        let retrieved_plan = manager.get_plan(session_id).await.unwrap();
        assert_eq!(retrieved_plan.entries.len(), 1);
        
        // 更新条目状态
        manager.update_entry_status(session_id, &entry_id, PlanEntryStatus::InProgress).await.unwrap();
        
        // 验证状态更新事件
        let event = timeout(Duration::from_millis(100), event_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(event, PlanEvent::EntryStatusUpdated { .. }));
        
        // 获取统计信息
        let stats = manager.get_plan_stats(session_id).await.unwrap();
        assert_eq!(stats.in_progress, 1);
        assert_eq!(stats.pending, 0);
    }
    
    #[tokio::test]
    async fn test_frontend_update_subscription() {
        let (manager, _event_rx) = PlanManager::new();
        let session_id = "test_session";
        
        // 订阅更新
        let mut update_rx = manager.subscribe_updates().await;
        
        // 创建Plan
        let plan = Plan::new();
        manager.update_plan(session_id, plan).await.unwrap();
        
        // 验证前端更新事件
        let update = timeout(Duration::from_millis(100), update_rx.recv()).await.unwrap().unwrap();
        assert_eq!(update.session_id, session_id);
        assert!(matches!(update.update_type, PlanUpdateType::FullUpdate));
    }
}
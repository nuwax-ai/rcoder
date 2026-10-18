//! 定期清理闲置agent的任务
//!
//! 基于AgentLifecycleGuard的RAII原则，简化清理逻辑：
//! 1. 定时扫描识别闲置的agent
//! 2. 从PROJECT_AND_AGENT_INFO_MAP中移除
//! 3. AgentLifecycleGuard自动drop并清理资源

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::model::AgentStatus;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER, SESSION_CACHE};

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 闲置超时时间（默认30分钟）
    pub idle_timeout: Duration,
    /// 清理检查间隔（默认5分钟）
    pub cleanup_interval: Duration,
    // 注意：force_terminate_timeout 字段已移除
    // 因为采用RAII模式，AgentLifecycleGuard会自动处理资源清理
    // 不需要强制终止超时机制
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(3 * 60), // 🆕 默认3分钟
            cleanup_interval: Duration::from_secs(30), // 默认30秒
        }
    }
}

/// 清理任务统计信息
#[derive(Debug, Clone, Default)]
pub struct CleanupStats {
    /// 总共清理的agent数量
    pub total_cleaned: u64,
    /// 成功清理的agent数量
    pub success_cleaned: u64,
    /// 清理失败的agent数量
    pub failed_cleaned: u64,
    /// 清理的孤立session数量
    pub orphaned_sessions_cleaned: u64,
    /// 清理的SSE消息数量
    pub sse_messages_cleaned: u64,
    /// 最后清理时间
    pub last_cleanup: Option<DateTime<Utc>>,
}

/// Agent清理器 - 基于RAII的简化版本
pub struct AgentCleaner {
    config: CleanupConfig,
    stats: CleanupStats,
}

impl AgentCleaner {
    /// 创建新的清理器
    pub fn new(config: CleanupConfig) -> Self {
        Self {
            config,
            stats: CleanupStats::default(),
        }
    }

    /// 检查agent是否闲置超时
    fn is_agent_idle_timeout(
        &self,
        last_activity: DateTime<Utc>,
        current_time: DateTime<Utc>,
    ) -> bool {
        let duration = current_time.signed_duration_since(last_activity);
        duration.num_seconds() > 0
            && duration.num_seconds() as u64 > self.config.idle_timeout.as_secs()
    }

    /// 清理孤立的SSE消息数据
    /// 清理没有project_id引用的session和长期未活跃的session
    async fn cleanup_orphaned_sse_sessions(&mut self) -> (u64, u64) {
        let mut orphaned_count = 0;
        let mut messages_cleared = 0;

        // 使用统一 Registry 收集所有活跃的 session_id
        let active_session_ids: std::collections::HashSet<String> = AGENT_REGISTRY
            .iter_agents()
            .filter_map(|entry| {
                shared_types::SessionHint::classify(Some(entry.value().session_id.0.as_ref()))
                    .as_session_id()
                    .map(str::to_owned)
            })
            .collect();

        // 检查SESSION_CACHE中的所有session
        let mut sessions_to_remove = Vec::new();

        let session_ids: Vec<String> = SESSION_CACHE
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for session_id in session_ids {
            // 如果session_id不在活跃映射中，则为孤立session
            if !active_session_ids.contains(&session_id) {
                // 检查session中是否有消息
                // view() 在闭包返回后立即释放锁，无 Ref 暴露
                if let Some(session_data) = SESSION_CACHE.view(&session_id, |_, d| d.clone()) {
                    let message_count = session_data.message_count().await;

                    if message_count > 0 {
                        info!(
                            "Found orphaned session: session_id={}, message_count={}",
                            session_id, message_count
                        );

                        // 清理这个session的消息 - 直接移除条目
                        if SESSION_CACHE.remove(&session_id).is_some() {
                            messages_cleared += 1;
                        }

                        // 如果清理后session为空，标记为待删除
                        if session_data.message_count().await == 0 {
                            sessions_to_remove.push(session_id.clone());
                        }

                        orphaned_count += 1;
                    } else {
                        // 没有消息的空session，直接标记删除
                        sessions_to_remove.push(session_id.clone());
                    }
                } else {
                    // session_data不存在，也标记删除
                    sessions_to_remove.push(session_id.clone());
                }
            }
        }

        // 删除空的孤立session
        for session_id in sessions_to_remove {
            if let Some((_, _)) = SESSION_CACHE.remove(&session_id) {
                debug!("Removed empty session: {}", session_id);
            }
        }

        if orphaned_count > 0 {
            info!(
                "Orphaned SSE session cleanup completed: session_count={}, message_count={}",
                orphaned_count, messages_cleared
            );
        }

        (orphaned_count, messages_cleared)
    }

    /// 执行一次清理操作 - 基于RAII的简化版
    /// 只需要从 MAP 中移除闲置agent，AgentLifecycleGuard 会自动清理资源
    async fn cleanup_idle_agents(&mut self) -> Result<CleanupStats> {
        let current_time = Utc::now();
        let mut cleaned_count = 0;
        let mut success_count = 0;
        let mut failed_count = 0;

        // 使用统一 Registry 获取统计信息
        let registry_stats = AGENT_REGISTRY.stats();
        let total_agents = registry_stats.agent_count;

        info!(
            "Starting cleanup for idle agents and SSE messages, current_time: {}, active_agent_count: {}",
            current_time, total_agents
        );

        // 先清理孤立的SSE消息数据
        let (orphaned_sessions, sse_messages) = self.cleanup_orphaned_sse_sessions().await;

        // 收集需要清理的agent ID（使用统一 Registry 遍历）
        let mut agents_to_remove = Vec::new();

        for entry in AGENT_REGISTRY.iter_agents() {
            let project_id = entry.key();
            let agent_info = entry.value();

            // 只清理Idle状态的agent，避免中断正在执行的任务
            if agent_info.status == AgentStatus::Idle
                && self.is_agent_idle_timeout(agent_info.last_activity, current_time)
            {
                let idle_duration = (current_time - agent_info.last_activity).num_seconds();
                info!(
                    "Found idle agent: project_id={}, status={:?}, last_activity: {}, idle_duration_seconds: {}, created_at: {}",
                    project_id,
                    agent_info.status,
                    agent_info.last_activity,
                    idle_duration,
                    agent_info.created_at
                );
                agents_to_remove.push((project_id.clone(), agent_info.clone()));
            }
        }

        // 保持原执行注册到真实退出，随后条件移除；扫描后的新回合不能被旧清理接管。
        for (project_id, captured) in agents_to_remove {
            match self
                .cleanup_agent_explicit_stop(&project_id, &captured)
                .await
            {
                Ok(true) => {
                    success_count += 1;
                    info!("Agent cleaned successfully: {}", project_id);
                }
                Ok(false) => continue,
                Err(e) => {
                    failed_count += 1;
                    warn!("Failed to clean agent: {} - {}", project_id, e);
                }
            }
            cleaned_count += 1;
        }

        // 更新统计信息
        self.stats.total_cleaned += cleaned_count;
        self.stats.success_cleaned += success_count;
        self.stats.failed_cleaned += failed_count;
        self.stats.orphaned_sessions_cleaned += orphaned_sessions;
        self.stats.sse_messages_cleaned += sse_messages;
        self.stats.last_cleanup = Some(current_time);

        // 清理完成后的统计（使用统一 Registry）
        let final_stats = AGENT_REGISTRY.stats();
        let remaining_agents = final_stats.agent_count;
        let active_sessions = final_stats.session_count;
        let cached_sessions = SESSION_CACHE.len();

        info!(
            "Cleanup completed: agent(total={}, success={}, failed={}, remaining={}) | session(active={}, cached={}) | sse_messages(cleared={})",
            cleaned_count,
            success_count,
            failed_count,
            remaining_agents,
            active_sessions,
            cached_sessions,
            sse_messages
        );

        Ok(CleanupStats {
            total_cleaned: cleaned_count,
            success_cleaned: success_count,
            failed_cleaned: failed_count,
            orphaned_sessions_cleaned: orphaned_sessions,
            sse_messages_cleaned: sse_messages,
            last_cleanup: Some(current_time),
        })
    }

    /// 显式停止 Agent 子进程并清理 Registry。
    ///
    /// Registry 中的 lifecycle handle 只是一个 Arc 克隆，删除 Registry 不能保证这是最后
    /// 一个引用。这里主动调用 graceful_stop，避免 idle cleanup 后子进程继续存活。
    async fn cleanup_agent_explicit_stop(
        &self,
        project_id: &str,
        captured: &shared_types::ProjectAndAgentInfo,
    ) -> Result<bool> {
        let _transition = agent_abstraction::session::lock_execution_transition(project_id).await;
        let session_id = captured.session_id.to_string();
        let admitted = AGENT_REGISTRY.try_update_agent_info(project_id, |current| {
            if current.session_id.to_string() == session_id
                && current.cancel_tx.same_channel(&captured.cancel_tx)
                && current.status == AgentStatus::Idle
                && current.last_activity == captured.last_activity
            {
                current.status = AgentStatus::Terminating;
                true
            } else {
                false
            }
        });
        if !admitted {
            return Ok(false);
        }
        if let Some(handle) = &captured.stop_handle {
            handle.graceful_stop().await?;
            handle.wait_for_exit().await?;
        } else if !captured.prompt_tx.is_closed() {
            anyhow::bail!("idle agent has no lifecycle handle: {project_id}/{session_id}");
        }
        // 同步清理期间持有匹配执行的读锁；没有跨 await，防同 sid 新进程的权限被误删。
        if let Some(current) = AGENT_REGISTRY.get_agent_info(project_id)
            && current.session_id.to_string() == session_id
            && current.cancel_tx.same_channel(&captured.cancel_tx)
        {
            if AGENT_REGISTRY
                .get_project_by_session(&session_id)
                .as_deref()
                == Some(project_id)
            {
                PERMISSION_MANAGER.cancel_session_permissions(&session_id);
                PERMISSION_MANAGER.clear_session_state(&session_id);
            }
            if let Some(request_id) = captured.request_id.as_deref() {
                crate::proxy_agent::SESSION_REQUEST_CONTEXT
                    .remove_if(project_id, |_, current| current == request_id);
            }
        }
        let removed = AGENT_REGISTRY.remove_by_project_if_session_and_channel_matches(
            project_id,
            &session_id,
            &captured.cancel_tx,
        );
        Ok(removed.is_some())
    }

    /// 运行清理任务 - 简化版，只做定时清理
    pub async fn run(&mut self) {
        info!("Cleanup task started, config: {:?}", self.config);

        let mut interval = tokio::time::interval(self.config.cleanup_interval);

        loop {
            interval.tick().await;

            match self.cleanup_idle_agents().await {
                Ok(stats) => debug!("Periodic cleanup completed: {:?}", stats),
                Err(e) => warn!("Periodic cleanup failed: {}", e),
            }
        }
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> &CleanupStats {
        &self.stats
    }
}

/// 启动清理任务 - 普通异步版本
///
/// 清理任务只操作 Send 数据结构，可以在普通异步线程中运行
pub fn start_cleanup_task(config: CleanupConfig) -> tokio::task::JoinHandle<()> {
    let mut cleaner = AgentCleaner::new(config);

    tokio::task::spawn(async move {
        cleaner.run().await;
    })
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionId;

    #[tokio::test]
    async fn stale_idle_scan_preserves_a_replacement_with_the_same_sid() {
        let project = format!("idle-scan-{}", uuid::Uuid::new_v4());
        let (prompt_tx, _prompt_receiver) = tokio::sync::mpsc::channel(1);
        let (cancel_tx, _cancel_receiver) = tokio::sync::mpsc::channel(1);
        let mut first = shared_types::ProjectAndAgentInfo {
            project_id: project.clone(),
            session_id: SessionId::from("idle-scan-session"),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: Utc::now(),
            created_at: Utc::now(),
            stop_handle: None,
            agent_binary_snapshot: None,
        };
        let (old_cancel, _old_receiver) = tokio::sync::mpsc::channel(1);
        first.cancel_tx = old_cancel;
        AGENT_REGISTRY.register(&project, "idle-scan-session", first.clone());
        let (new_cancel, _new_receiver) = tokio::sync::mpsc::channel(1);
        let mut replacement = first.clone();
        replacement.cancel_tx = new_cancel.clone();
        AGENT_REGISTRY.register(&project, "idle-scan-session", replacement);
        let cleaner = AgentCleaner::new(CleanupConfig::default());
        assert!(
            !cleaner
                .cleanup_agent_explicit_stop(&project, &first)
                .await
                .unwrap()
        );
        assert!(
            AGENT_REGISTRY
                .view_agent_info(&project, |info| info.cancel_tx.same_channel(&new_cancel))
                .unwrap()
        );
        AGENT_REGISTRY.remove_by_project(&project);
    }
}

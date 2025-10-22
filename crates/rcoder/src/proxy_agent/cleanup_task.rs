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

use crate::AgentStatus;
use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;
use crate::service::session_cache::{SESSION_CACHE, PROJECT_SESSION_MAP};

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
            idle_timeout: Duration::from_secs(30 * 60),
            cleanup_interval: Duration::from_secs(5 * 60),
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

        // 收集所有活跃的session_id（从PROJECT_SESSION_MAP中获取）
        let active_session_ids: std::collections::HashSet<String> = PROJECT_SESSION_MAP
            .iter()
            .map(|entry| entry.value().clone())
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
                if let Some(session_data_ref) = SESSION_CACHE.get(&session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref);

                    let message_count = session_data.message_count().await;

                    if message_count > 0 {
                        info!(
                            "发现孤立session: session_id={}, 消息数量={}",
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
                debug!("移除空session: {}", session_id);
            }
        }

        if orphaned_count > 0 {
            info!(
                "清理孤立SSE session完成: session数量={}, 消息数量={}",
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

        // 统计当前活动的agent数量
        let total_agents = PROJECT_AND_AGENT_INFO_MAP.len();

        info!(
            "开始清理闲置agent和SSE消息，当前时间: {}，当前活动agent数量: {}",
            current_time, total_agents
        );

        // 先清理孤立的SSE消息数据
        let (orphaned_sessions, sse_messages) = self.cleanup_orphaned_sse_sessions().await;

        // 收集需要清理的agent ID
        let mut agents_to_remove = Vec::new();

        for entry in PROJECT_AND_AGENT_INFO_MAP.iter() {
            let project_id = entry.key();
            let agent_info = entry.value();

            // 只清理Idle状态的agent，避免中断正在执行的任务
            if agent_info.status == AgentStatus::Idle
                && self.is_agent_idle_timeout(agent_info.last_activity, current_time)
            {
                let idle_duration = (current_time - agent_info.last_activity).num_seconds();
                info!(
                    "发现闲置agent: project_id={}, 状态={:?}, 最后活动: {}, 闲置时长: {}秒, 创建时间: {}",
                    project_id,
                    agent_info.status,
                    agent_info.last_activity,
                    idle_duration,
                    agent_info.created_at
                );
                agents_to_remove.push(project_id.clone());
            }
        }

        // 执行清理 - RAII版：直接从 MAP 中移除，AgentLifecycleGuard 会自动清理
        for project_id in agents_to_remove {
            match self.cleanup_agent_raii(&project_id) {
                Ok(_) => {
                    success_count += 1;
                    info!("成功清理agent: {}", project_id);
                }
                Err(e) => {
                    failed_count += 1;
                    warn!("清理agent失败: {} - {}", project_id, e);
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

        // 清理完成后的统计
        let remaining_agents = PROJECT_AND_AGENT_INFO_MAP.len();
        let active_sessions = PROJECT_SESSION_MAP.len();
        let cached_sessions = SESSION_CACHE.len();

        info!(
            "清理完成: agent(总共={}, 成功={}, 失败={}, 剩余={}) | session(活跃={}, 缓存={}) | SSE消息(清理={})",
            cleaned_count, success_count, failed_count, remaining_agents,
            active_sessions, cached_sessions, sse_messages
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

    /// 基于RAII的简化清理方法
    /// 只需要从MAP中移除agent，AgentLifecycleGuard会自动清理所有资源
    fn cleanup_agent_raii(&self, project_id: &str) -> Result<()> {
        debug!("开始RAII清理agent: {}", project_id);

        // 检查agent是否存在
        if PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id) {
            // 直接从MAP中移除，触发AgentLifecycleGuard的Drop
            let removed = PROJECT_AND_AGENT_INFO_MAP.remove(project_id);

            // 同步清理 SESSION_REQUEST_CONTEXT 中的 request_id
            crate::proxy_agent::SESSION_REQUEST_CONTEXT.remove(project_id);
            debug!("🧼 [cleanup] 已清理 SESSION_REQUEST_CONTEXT 中的 project_id={}", project_id);

            // 清理 PROJECT_SESSION_MAP 中的映射关系
            if let Some((_, removed_session_id)) = PROJECT_SESSION_MAP.remove(project_id) {
                debug!("🧼 [cleanup] 已清理 PROJECT_SESSION_MAP 映射: project_id={}, session_id={}",
                       project_id, removed_session_id);
            }

            if removed.is_some() {
                info!(
                    "Agent已从MAP中移除，AgentLifecycleGuard将自动清理资源: {}",
                    project_id
                );
            } else {
                warn!("尝试移除agent但未找到: {}", project_id);
            }
        } else {
            warn!("Agent不存在于MAP中: {}", project_id);
        }

        Ok(())
    }

    /// 运行清理任务 - 简化版，只做定时清理
    pub async fn run(&mut self) {
        info!("清理任务已启动，配置: {:?}", self.config);

        let mut interval = tokio::time::interval(self.config.cleanup_interval);

        loop {
            interval.tick().await;

            match self.cleanup_idle_agents().await {
                Ok(stats) => debug!("定时清理完成: {:?}", stats),
                Err(e) => warn!("定时清理失败: {}", e),
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

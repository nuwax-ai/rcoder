//! 定期清理闲置agent的任务
//!
//! 基于AgentLifecycleGuard的RAII原则，简化清理逻辑：
//! 1. 定时扫描识别闲置的agent
//! 2. 从state.project_and_agent_map中移除
//! 3. AgentLifecycleGuard自动drop并清理资源

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::AgentStatus;
use crate::router::AppState;
use shared_types::ProjectAndContainerInfo;

/// 🆕 Agent信息访问trait，用于统一不同类型的agent信息访问接口
trait AgentInfoAccess {
    fn project_id(&self) -> &str;
    fn last_activity(&self) -> DateTime<Utc>;
    fn created_at(&self) -> DateTime<Utc>;
    fn status(&self) -> Option<AgentStatus>;
}

/// 为ProjectAndContainerInfo实现AgentInfoAccess trait
impl AgentInfoAccess for ProjectAndContainerInfo {
    fn project_id(&self) -> &str {
        // 使用公共方法，避免递归调用
        ProjectAndContainerInfo::project_id(self)
    }

    fn last_activity(&self) -> DateTime<Utc> {
        ProjectAndContainerInfo::last_activity(self)
    }

    fn created_at(&self) -> DateTime<Utc> {
        ProjectAndContainerInfo::created_at(self)
    }

    fn status(&self) -> Option<AgentStatus> {
        // AgentStatus实现了Copy，可以直接解引用
        ProjectAndContainerInfo::status(self).copied()
    }
}

/// 为Arc<ProjectAndContainerInfo>实现AgentInfoAccess trait
impl AgentInfoAccess for Arc<ProjectAndContainerInfo> {
    fn project_id(&self) -> &str {
        ProjectAndContainerInfo::project_id(self)
    }

    fn last_activity(&self) -> DateTime<Utc> {
        ProjectAndContainerInfo::last_activity(self)
    }

    fn created_at(&self) -> DateTime<Utc> {
        ProjectAndContainerInfo::created_at(self)
    }

    fn status(&self) -> Option<AgentStatus> {
        ProjectAndContainerInfo::status(self).copied()
    }
}

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 闲置超时时间（默认30分钟）
    pub idle_timeout: Duration,
    /// 清理检查间隔（默认5分钟）
    pub cleanup_interval: Duration,
    /// Docker容器停止超时时间（默认30秒）
    pub docker_stop_timeout: Duration,
    // 注意：force_terminate_timeout 字段已移除
    // 因为采用RAII模式，AgentLifecycleGuard会自动处理资源清理
    // 不需要强制终止超时机制
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30 * 60),
            cleanup_interval: Duration::from_secs(5 * 60),
            docker_stop_timeout: Duration::from_secs(30),
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
    state: Arc<AppState>,
}

impl AgentCleaner {
    /// 创建新的清理器
    pub fn new(config: CleanupConfig, state: Arc<AppState>) -> Self {
        Self {
            config,
            stats: CleanupStats::default(),
            state,
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

    /// 🛡️ 通用容器保护时间检查函数
    ///
    /// 统一的保护逻辑，避免新创建的容器被误清理：
    /// 1. 检查容器创建时间是否在保护期内
    /// 2. 返回是否应该跳过清理
    fn should_skip_cleanup_due_to_protection(
        &self,
        created_at: DateTime<Utc>,
        project_id: &str,
    ) -> bool {
        // 🛡️ 最小保护时间：容器创建后5分钟内不会被清理
        let MIN_PROTECTION_DURATION = Duration::from_secs(5 * 60);

        let current_time = Utc::now();
        let age = current_time.signed_duration_since(created_at);

        if age.num_seconds() < MIN_PROTECTION_DURATION.as_secs() as i64 {
            info!(
                "🛡️ [cleanup] 容器在保护期内，跳过清理: project_id={}, 创建时长={}秒",
                project_id,
                age.num_seconds()
            );
            return true;
        }

        false
    }

    /// 🆕 改进的超时判断函数，包含创建时间保护
    ///
    /// 这个函数解决了新创建容器被立即清理的问题：
    /// 1. 检查last_activity超时
    /// 2. 确保容器存在最小保护时间
    /// 3. 避免时间计算误差导致的误清理
    fn is_agent_idle_timeout_with_protection(
        &self,
        agent_info: &impl AgentInfoAccess,
        current_time: DateTime<Utc>,
    ) -> bool {
        // 1. 检查创建时间保护期（使用统一的保护逻辑）
        if self
            .should_skip_cleanup_due_to_protection(agent_info.created_at(), agent_info.project_id())
        {
            return false;
        }

        // 2. 检查闲置超时（添加1秒的缓冲时间避免时间误差）
        let last_activity = agent_info.last_activity();
        let idle_duration = current_time.signed_duration_since(last_activity);
        let idle_timeout_with_buffer = self.config.idle_timeout.as_secs() + 1;

        let is_timeout = idle_duration.num_seconds() > 0
            && idle_duration.num_seconds() as u64 > idle_timeout_with_buffer;

        debug!(
            "🕒 [cleanup] 闲置检查: project_id={}, 最后活动={}, 闲置时长={}秒, 超时阈值={}秒, 是否超时={}",
            agent_info.project_id(),
            last_activity,
            idle_duration.num_seconds(),
            idle_timeout_with_buffer,
            is_timeout
        );

        is_timeout
    }

    /// 清理孤立的SSE消息数据
    /// 清理没有在 project_and_agent_map 中对应session_id的条目
    async fn cleanup_orphaned_sse_sessions(&mut self) -> (u64, u64) {
        let mut orphaned_count = 0;
        let mut messages_cleared = 0;

        // 收集所有活跃的session_id（从 project_and_agent_map 中获取）
        let active_session_ids: std::collections::HashSet<String> = self
            .state
            .project_and_agent_map
            .iter()
            .filter_map(|entry| entry.value().session_id().map(|s| s.to_string()))
            .collect();

        // 检查sessions中的所有session
        let mut sessions_to_remove = Vec::new();

        let session_ids: Vec<String> = self
            .state
            .sessions
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for session_id in session_ids {
            // 如果session_id不在活跃映射中，则为孤立session
            if !active_session_ids.contains(&session_id) {
                info!("发现孤立session: session_id={}", session_id);

                // 清理这个session
                if self.state.sessions.remove(&session_id).is_some() {
                    orphaned_count += 1;
                    messages_cleared += 1;
                }

                sessions_to_remove.push(session_id.clone());
            }
        }

        if orphaned_count > 0 {
            info!("清理孤立SSE session完成: session数量={}", orphaned_count);
        }

        (orphaned_count, messages_cleared)
    }

    /// 执行一次清理操作 - 基于RAII的简化版
    /// 只需要从 MAP 中移除闲置agent，AgentLifecycleGuard 会自动清理资源
    async fn cleanup_idle_agents(&mut self) -> Result<CleanupStats> {
        let start_time = std::time::Instant::now();
        let current_time = Utc::now();
        let mut cleaned_count = 0;
        let mut success_count = 0;
        let mut failed_count = 0;

        // 统计当前活动的agent数量
        let total_agents = self.state.project_and_agent_map.len();

        info!(
            "开始清理闲置agent和SSE消息，当前时间: {}，当前活动agent数量: {}",
            current_time, total_agents
        );

        // 先清理孤立的SSE消息数据
        let (orphaned_sessions, sse_messages) = self.cleanup_orphaned_sse_sessions().await;

        // 收集需要清理的agent ID
        let mut agents_to_remove = Vec::new();

        // 📊 统计各类agent数量
        let protected_agents = 0;
        let mut active_agents = 0;
        let mut non_timeout_agents = 0;

        info!(
            "🔍 [cleanup] 开始扫描所有agent，总数: {}",
            self.state.project_and_agent_map.len()
        );

        // 🔒 使用原子性操作收集需要清理的agent，避免长时间持有读锁
        let mut agents_to_evaluate = Vec::new();

        // 先快速收集所有项目ID，避免长时间迭代
        for entry in self.state.project_and_agent_map.iter() {
            agents_to_evaluate.push(entry.key().clone());
        }

        // 现在逐个检查每个agent，使用原子性操作
        for project_id in agents_to_evaluate {
            // 使用 Entry API 原子性地检查和标记待清理的agent
            if let Some(agent_ref) = self.state.project_and_agent_map.get(&project_id) {
                let status = agent_ref.status();
                let last_activity = agent_ref.last_activity();
                let created_at = agent_ref.created_at();

                // 立即释放引用，避免长时间持有
                drop(agent_ref);

                // 🎯 状态检查逻辑：
                let should_clean_by_status = match status {
                    Some(AgentStatus::Idle) => {
                        debug!(
                            "✅ [cleanup] 状态检查通过: project_id={}, 状态=Idle",
                            project_id
                        );
                        true
                    }
                    None => {
                        debug!(
                            "✅ [cleanup] 状态检查通过: project_id={}, 状态=None(新创建)",
                            project_id
                        );
                        true // 新创建的容器状态为None，也应该被检查
                    }
                    Some(AgentStatus::Active) => {
                        debug!(
                            "⏸️ [cleanup] 跳过Active状态agent: project_id={}",
                            project_id
                        );
                        active_agents += 1;
                        false
                    }
                    Some(AgentStatus::Terminating) => {
                        debug!(
                            "🔄 [cleanup] 跳过Terminating状态agent: project_id={}",
                            project_id
                        );
                        false
                    }
                };

                if should_clean_by_status {
                    // 超时检查
                    let idle_duration = current_time - last_activity;
                    let age = current_time - created_at;

                    let is_timeout = idle_duration
                        > chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default();
                    let is_protected =
                        self.should_skip_cleanup_due_to_protection(created_at, &project_id);

                    if is_timeout && !is_protected {
                        let idle_duration_secs = idle_duration.num_seconds();
                        let age_secs = age.num_seconds();

                        info!(
                            "🎯 [cleanup] 发现待清理agent: project_id={}, 状态={:?}, 最后活动={}, 闲置时长={}秒, 创建时长={}秒, 创建时间={}",
                            project_id,
                            status,
                            last_activity,
                            idle_duration_secs,
                            age_secs,
                            created_at
                        );
                        agents_to_remove.push(project_id);
                    } else {
                        non_timeout_agents += 1;
                        debug!(
                            "⏰ [cleanup] Agent未超时或在保护期，跳过清理: project_id={}",
                            project_id
                        );
                    }
                }
            }
        }

        info!(
            "📊 [cleanup] 扫描完成: 总数={}, 待清理={}, 保护期内={}, 活跃状态={}, 未超时={}",
            self.state.project_and_agent_map.len(),
            agents_to_remove.len(),
            protected_agents,
            active_agents,
            non_timeout_agents
        );

        // 执行清理 - RAII版：先销毁Docker容器，再从 MAP 中移除，AgentLifecycleGuard 会自动清理其他资源
        for project_id in agents_to_remove {
            match self.cleanup_agent_raii(&project_id).await {
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

        // 🆕 清理孤立的容器（MAP中没有记录但容器还在运行的情况）
        let orphaned_containers = self.cleanup_orphaned_containers().await;

        // 更新统计信息
        self.stats.total_cleaned += cleaned_count;
        self.stats.success_cleaned += success_count;
        self.stats.failed_cleaned += failed_count;
        self.stats.orphaned_sessions_cleaned += orphaned_sessions;
        self.stats.sse_messages_cleaned += sse_messages;
        self.stats.last_cleanup = Some(current_time);

        // 清理完成后的统计
        let remaining_agents = self.state.project_and_agent_map.len();
        let active_sessions = self.state.sessions.len();

        let duration = start_time.elapsed();
        info!(
            "清理完成 - 清理数量: {}, 成功: {}, 失败: {}, 孤立SSE会话: {}, SSE消息: {}, 孤立容器: {}, 剩余agent: {}, 活跃会话: {}, 耗时: {:.2}秒",
            cleaned_count,
            success_count,
            failed_count,
            orphaned_sessions,
            sse_messages,
            orphaned_containers,
            remaining_agents,
            active_sessions,
            duration.as_secs_f64()
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

    /// 🆕 清理孤立的容器 - 优化版本
    /// 使用更高效的方式清理孤立容器，避免阻塞主线程
    async fn cleanup_orphaned_containers(&self) -> u64 {
        info!("🔍 开始检查孤立的容器");

        // 🚀 优化6: 为整个孤立容器清理添加总超时时间
        // 超时时间应该远小于清理间隔（5分钟），设置为2分钟
        let total_timeout = Duration::from_secs(120); // 2分钟总超时

        match timeout(total_timeout, self.cleanup_orphaned_containers_inner()).await {
            Ok(cleaned_count) => {
                info!("✅ 孤立容器清理完成，清理了 {} 个容器", cleaned_count);
                cleaned_count
            }
            Err(_) => {
                warn!(
                    "⏰ 孤立容器清理超时，耗时超过 {} 秒，强制结束",
                    total_timeout.as_secs()
                );
                0
            }
        }
    }

    /// 内部清理方法，不包含总超时
    async fn cleanup_orphaned_containers_inner(&self) -> u64 {
        // 获取全局 DockerManager
        let docker_manager = match docker_manager::global::get_global_docker_manager().await {
            Ok(manager) => manager,
            Err(e) => {
                warn!("获取全局 DockerManager 失败，跳过孤立容器清理: {}", e);
                return 0;
            }
        };

        // 🚀 优化1: 使用更快的容器列表查询，只获取基本信息
        let containers = match docker_manager
            .list_containers_with_pattern("rcoder-agent-*")
            .await
        {
            Ok(containers) => containers,
            Err(e) => {
                warn!("列出容器失败，跳过孤立容器清理: {}", e);
                return 0;
            }
        };

        if containers.is_empty() {
            debug!("✅ 未发现任何 rcoder-agent 容器");
            return 0;
        }

        // 🚀 优化2: 批量处理，减少Docker API调用次数
        let mut orphaned_containers = Vec::new();
        let mut protected_count = 0;

        // 快速筛选孤立容器（不进行详细的Docker查询）
        for container in containers {
            if let Some(names) = &container.names {
                for name in names {
                    let clean_name = name.trim_start_matches('/');
                    if let Some(project_id) = clean_name.strip_prefix("rcoder-agent-") {
                        // 检查 MAP 中是否有对应记录
                        if !self.state.project_and_agent_map.contains_key(project_id) {
                            // 🚀 优化3: 使用容器的创建时间而不是查询详细信息
                            if let Some(created_at) = container.created {
                                let created_time = DateTime::from_timestamp(created_at, 0)
                                    .unwrap_or_else(Utc::now);

                                if self
                                    .should_skip_cleanup_due_to_protection(created_time, project_id)
                                {
                                    protected_count += 1;
                                    debug!("🛡️ 容器在保护期内，跳过: {}", clean_name);
                                    break;
                                }
                            }

                            orphaned_containers
                                .push((project_id.to_string(), clean_name.to_string()));
                            break; // 找到匹配的名称后跳出内层循环
                        }
                    }
                }
            }
        }

        if orphaned_containers.is_empty() {
            debug!("✅ 未发现孤立容器");
            return 0;
        }

        info!("🗑️ 发现 {} 个孤立容器，开始清理", orphaned_containers.len());

        // 🚀 优化4: 限制单次清理数量，避免长时间阻塞
        let max_cleanup_per_round = 5; // 减少到5个，避免阻塞
        let total_orphaned = orphaned_containers.len();
        let containers_to_clean = orphaned_containers
            .into_iter()
            .take(max_cleanup_per_round)
            .collect::<Vec<_>>();

        if containers_to_clean.len() < total_orphaned {
            info!(
                "🔒 限制单次清理数量为 {}，剩余容器将在下次清理",
                max_cleanup_per_round
            );
        }

        // 🚀 优化5: 并行清理容器，提高效率
        let mut cleanup_tasks = Vec::new();

        for (project_id, container_name) in containers_to_clean {
            let docker_manager_clone = docker_manager.clone();
            let state_clone = self.state.clone();
            let config = self.config.clone();

            let task = tokio::spawn(async move {
                let cleanup_timeout = Duration::from_secs(30);
                match timeout(
                    cleanup_timeout,
                    Self::cleanup_single_orphaned_container(
                        &docker_manager_clone,
                        &state_clone,
                        &config,
                        &project_id,
                        &container_name,
                    ),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        info!("✅ 成功清理孤立容器: {}", container_name);
                        true
                    }
                    Ok(Err(e)) => {
                        warn!("❌ 清理孤立容器失败: {} - {}", container_name, e);
                        false
                    }
                    Err(_) => {
                        warn!(
                            "⏰ 清理孤立容器超时: {} (超时时间: {}秒)",
                            container_name,
                            cleanup_timeout.as_secs()
                        );
                        false
                    }
                }
            });

            cleanup_tasks.push(task);
        }

        // 等待所有清理任务完成
        let mut cleaned_count = 0;
        for task in cleanup_tasks {
            if let Ok(success) = task.await
                && success {
                    cleaned_count += 1;
                }
        }

        if cleaned_count > 0 || protected_count > 0 {
            info!(
                "🧹 孤立容器检查完成: 共清理 {} 个容器，保护期内 {} 个容器",
                cleaned_count, protected_count
            );
        }

        cleaned_count
    }

    /// 清理单个孤立容器
    /// 
    /// 使用统一的运行时清理策略
    async fn cleanup_single_orphaned_container(
        docker_manager: &Arc<docker_manager::DockerManager>,
        _state: &Arc<AppState>,
        _config: &CleanupConfig,
        project_id: &str,
        container_name: &str,
    ) -> Result<()> {
        info!(
            "🔥 开始清理孤立容器: {} (project_id={})",
            container_name, project_id
        );

        // 查找容器信息
        let container_info = match docker_manager
            .find_container_by_identifier(container_name)
            .await
        {
            Some(info) => info,
            None => {
                info!("📭 容器不存在，无需清理: {}", container_name);
                return Ok(());
            }
        };

        // 使用统一的运行时清理接口
        docker_manager::container_stop::runtime_cleanup_container(
            docker_manager,
            &container_info.container_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("清理容器失败: {}", e))?;

        info!("✅ 容器清理成功: {}", container_name);
        Ok(())
    }

    /// 基于RAII的简化清理方法
    /// 先销毁Docker容器，再从MAP中移除agent，AgentLifecycleGuard会自动清理其他资源
    async fn cleanup_agent_raii(&self, project_id: &str) -> Result<()> {
        info!("🚀 [cleanup_agent_raii] 开始RAII清理agent: {}", project_id);

        // 为整个清理过程添加超时机制，防止无限阻塞
        let cleanup_result = tokio::time::timeout(
            Duration::from_secs(60), // 60秒总超时
            async {
                info!("📋 [cleanup_agent_raii] 步骤1: 开始销毁Docker容器: {}", project_id);

                // 首先销毁Docker容器（如果存在）
                if let Err(e) = self.destroy_docker_container(project_id).await {
                    warn!("⚠️ [cleanup_agent_raii] 销毁Docker容器失败: {} - {}", project_id, e);
                } else {
                    info!("✅ [cleanup_agent_raii] Docker容器销毁完成: {}", project_id);
                }

                info!("📋 [cleanup_agent_raii] 步骤2: 开始清理MAP映射: {}", project_id);

                // 🔒 使用 Entry API 实现原子性操作，避免读写锁竞争
                info!("🔍 [cleanup_agent_raii] 尝试原子性地移除agent: {}", project_id);
                let session_id_to_remove = match self.state.project_and_agent_map.entry(project_id.to_string()) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        info!("✅ [cleanup_agent_raii] 找到agent，提取session_id并原子性移除: {}", project_id);
                        let session_id = entry.get().session_id().map(|s| s.to_string());
                        info!("📝 [cleanup_agent_raii] 提取session_id: {:?}, project_id: {}", session_id, project_id);

                        // 原子性地移除条目，触发 AgentLifecycleGuard 的 Drop
                        entry.remove_entry();
                        info!("✅ [cleanup_agent_raii] 成功原子性移除agent: {}", project_id);
                        session_id
                    }
                    dashmap::mapref::entry::Entry::Vacant(_) => {
                        info!("📭 [cleanup_agent_raii] Agent不存在于MAP中，无需清理: {}", project_id);
                        return Ok(());
                    }
                };

                // 🗑️ 使用 Entry API 原子性地清理 sessions 映射关系
                if let Some(ref session_id) = session_id_to_remove {
                    info!("🗑️ [cleanup_agent_raii] 尝试原子性清理sessions映射: session_id={}, project_id={}", session_id, project_id);

                    match self.state.sessions.entry(session_id.clone()) {
                        dashmap::mapref::entry::Entry::Occupied(entry) => {
                            let removed_session_info = entry.get().clone();
                            entry.remove_entry();
                            info!(
                                "✅ [cleanup_agent_raii] 已原子性清理sessions映射: session_id={}, project_id={}",
                                session_id,
                                removed_session_info.project_id()
                            );
                        }
                        dashmap::mapref::entry::Entry::Vacant(_) => {
                            info!("📭 [cleanup_agent_raii] sessions映射不存在（可能已被清理）: session_id={}", session_id);
                        }
                    }
                } else {
                    info!("📭 [cleanup_agent_raii] 无session_id，跳过sessions清理: {}", project_id);
                }

                info!("✅ [cleanup_agent_raii] MAP清理完成，AgentLifecycleGuard将自动清理资源: {}", project_id);

                info!("🎯 [cleanup_agent_raii] 所有清理步骤完成: {}", project_id);
                Ok::<(), anyhow::Error>(())
            }
        ).await;

        match cleanup_result {
            Ok(Ok(())) => {
                info!("✅ [cleanup_agent_raii] 清理成功完成: {}", project_id);
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(
                    "⚠️ [cleanup_agent_raii] 清理过程中出错: {} - {}",
                    project_id, e
                );
                Err(e)
            }
            Err(_) => {
                error!(
                    "⏰ [cleanup_agent_raii] 清理操作超时 (60秒): {}",
                    project_id
                );
                Err(anyhow::anyhow!("清理操作超时: {}", project_id))
            }
        }
    }

    /// 销毁Docker容器
    /// 
    /// 使用统一的运行时清理策略
    async fn destroy_docker_container(&self, project_id: &str) -> Result<()> {
        info!("🔥 [cleanup] 开始销毁Docker容器: project_id={}", project_id);

        // 使用全局 DockerManager
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| anyhow::anyhow!("获取全局 DockerManager 失败: {}", e))?;

        // 1. 先通过 project_id 查找容器
        let mut container_info = docker_manager.get_container_info(project_id);

        // 2. 如果没找到，尝试通过容器名称查找
        if container_info.is_none() {
            let expected_container_name = format!("rcoder-agent-{}", project_id);
            container_info = docker_manager
                .find_container_by_identifier(&expected_container_name)
                .await;
        }

        if let Some(container_info) = container_info {
            info!(
                "🎯 [cleanup] 找到容器: project_id={}, container_id={}",
                project_id, container_info.container_id
            );

            // 释放端口（如果存在）
            if let Some(port_binding) = container_info.port_bindings.values().next()
                && let Ok(port) = port_binding.parse::<u16>() {
                    crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER
                        .release_port(port)
                        .await;
                    info!("🧼 [cleanup] 释放端口: port={}", port);
                }

            // 使用统一的运行时清理接口
            docker_manager::container_stop::runtime_cleanup_container(
                &docker_manager,
                &container_info.container_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("停止容器失败: {}", e))?;

            info!("✅ [cleanup] Docker容器销毁完成: project_id={}", project_id);
        } else {
            info!("📭 [cleanup] Docker容器不存在: project_id={}", project_id);
        }

        Ok(())
    }

    /// 运行清理任务 - 简化版，只做定时清理
    pub async fn run(&mut self) {
        info!("清理任务已启动，配置: {:?}", self.config);

        let mut interval = tokio::time::interval(self.config.cleanup_interval);

        loop {
            interval.tick().await;

            // 为整个清理任务添加超时保护，防止阻塞
            let cleanup_timeout = Duration::from_secs(120); // 2分钟超时
            let cleanup_result = timeout(cleanup_timeout, self.cleanup_idle_agents()).await;

            match cleanup_result {
                Ok(Ok(stats)) => {
                    debug!("定时清理完成: {:?}", stats);
                }
                Ok(Err(e)) => {
                    warn!("定时清理失败: {}", e);
                }
                Err(_) => {
                    warn!(
                        "定时清理超时，耗时超过{}秒，强制结束",
                        cleanup_timeout.as_secs()
                    );
                }
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
pub fn start_cleanup_task(
    config: CleanupConfig,
    state: Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    let mut cleaner = AgentCleaner::new(config, state);

    tokio::task::spawn(async move {
        cleaner.run().await;
    })
}

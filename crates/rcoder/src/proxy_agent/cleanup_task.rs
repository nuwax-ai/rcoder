//! 定期清理闲置agent的任务
//!
//! 基于AgentLifecycleGuard的RAII原则，简化清理逻辑：
//! 1. 定时扫描识别闲置的agent
//! 2. 从 DuckDB 存储中移除
//! 3. AgentLifecycleGuard自动drop并清理资源
//!
//! ## 支持的服务类型
//! - **RCoder**: project_id 闲置即清理
//! - **ComputerAgentRunner**: 销毁容器时同时清理 Pingora VNC 后端映射

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::AgentStatus;
use crate::router::AppState;
use shared_types::ProjectAndContainerInfo;
use shared_types::grpc::GetContainerStatusRequest;

/// 🆕 Agent信息访问trait，用于统一不同类型的agent信息访问接口
trait AgentInfoAccess {
    fn project_id(&self) -> &str;
    fn last_activity(&self) -> DateTime<Utc>;
    fn created_at(&self) -> DateTime<Utc>;
    fn status(&self) -> Option<AgentStatus>;
    fn user_id(&self) -> Option<String>;
    fn service_type(&self) -> Option<shared_types::ServiceType>;
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

    fn user_id(&self) -> Option<String> {
        ProjectAndContainerInfo::user_id(self).map(|s| s.to_string())
    }

    fn service_type(&self) -> Option<shared_types::ServiceType> {
        ProjectAndContainerInfo::service_type(self)
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

    fn user_id(&self) -> Option<String> {
        ProjectAndContainerInfo::user_id(self).map(|s| s.to_string())
    }

    fn service_type(&self) -> Option<shared_types::ServiceType> {
        ProjectAndContainerInfo::service_type(self)
    }
}

/// 孤立容器信息
///
/// 在筛选孤立容器时收集完整的上下文信息，避免后续重复解析
#[derive(Debug, Clone)]
struct OrphanedContainerInfo {
    /// 标识符（project_id 或 user_id，取决于服务类型）
    id: String,
    /// 容器名称
    container_name: String,
    /// 服务类型
    service_type: shared_types::ServiceType,
    /// 容器创建时间（如果可用）
    created_at: Option<DateTime<Utc>>,
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
    /// 🆕 预计算的容器模式列表（支持多服务类型）
    container_patterns: Vec<String>,
}

impl AgentCleaner {
    /// 创建新的清理器
    ///
    /// # Arguments
    ///
    /// * `config` - 清理配置
    /// * `state` - 应用状态
    /// * `multi_image_config` - 多镜像配置，用于获取启用的服务类型
    pub fn new(
        config: CleanupConfig,
        state: Arc<AppState>,
        multi_image_config: shared_types::MultiImageConfig,
    ) -> Self {
        // 预计算所有启用服务的容器模式
        let container_patterns =
            docker_manager::container_stop::get_container_patterns_for_enabled_services(
                &multi_image_config,
            );

        if container_patterns.is_empty() {
            warn!("⚠️ 没有启用的服务，孤立容器清理将跳过");
        } else {
            info!(
                "📋 [cleanup] 孤立容器清理将监控以下模式: {:?}",
                container_patterns
            );
        }

        Self {
            config,
            stats: CleanupStats::default(),
            state,
            container_patterns,
        }
    }

    /// 从容器模式推断服务类型
    ///
    /// 根据 docker_manager 返回的容器模式，推断对应的服务类型
    ///
    /// # 参数
    /// * `pattern` - 容器名称模式（如 "rcoder-agent-*", "computer-agent-runner-*"）
    ///
    /// # 返回
    /// 对应的 ServiceType，如果无法识别则返回 None
    fn infer_service_type_from_pattern(pattern: &str) -> Option<shared_types::ServiceType> {
        if pattern.starts_with("rcoder-agent-") || pattern.contains("rcoder-agent") {
            Some(shared_types::ServiceType::RCoder)
        } else if pattern.starts_with("computer-agent-runner-")
            || pattern.contains("computer-agent-runner")
        {
            Some(shared_types::ServiceType::ComputerAgentRunner)
        } else {
            // 未知模式
            None
        }
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
        let min_protection_duration = Duration::from_secs(5 * 60);

        let current_time = Utc::now();
        let age = current_time.signed_duration_since(created_at);

        if age.num_seconds() < min_protection_duration.as_secs() as i64 {
            info!(
                "🛡️ [cleanup] 容器在保护期内，跳过清理: project_id={}, 创建时长={}秒",
                project_id,
                age.num_seconds()
            );
            return true;
        }

        false
    }

    /// 清理孤立的SSE消息数据
    /// 清理没有在 projects 中对应session_id的条目
    /// 注意：使用 DuckDB 后，session 数据已合并到 projects 表，此方法主要用于兼容
    async fn cleanup_orphaned_sse_sessions(&mut self) -> (u64, u64) {
        let orphaned_count = 0;
        let messages_cleared = 0;

        // 使用 DuckDB 存储后，session 数据已合并到 projects 表
        // 孤立 session 会在项目删除时自动清理
        // 这里只记录日志用于监控
        debug!("🔍 [cleanup] 孤立会话检查完成（DuckDB 模式）");

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

        // 统计当前活动的agent数量（使用 DuckDB 存储）
        let total_agents = self.state.projects.len();

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
            self.state.projects.len()
        );

        // 🔒 使用原子性操作收集需要清理的agent，避免长时间持有读锁
        let mut agents_to_evaluate = Vec::new();

        // 先快速收集所有项目ID，避免长时间迭代（使用 DuckDB 存储）
        for (project_id, _) in self.state.projects.iter() {
            agents_to_evaluate.push(project_id);
        }

        // 现在逐个检查每个agent，使用原子性操作
        for project_id in agents_to_evaluate {
            // 使用 ProjectAdapter 获取 agent 信息
            if let Some(agent_ref) = self.state.get_project(&project_id) {
                let status = agent_ref.status();
                let last_activity = agent_ref.last_activity();
                let created_at = agent_ref.created_at();
                // 🆕 获取 user_id 和 service_type 用于更清晰的日志
                let user_id = agent_ref.user_id();
                let service_type = agent_ref.service_type();
                let container = agent_ref.container().cloned();

                // 立即释放引用，避免长时间持有
                drop(agent_ref);

                // 🎯 状态检查逻辑：
                let should_clean_by_status = match status {
                    Some(AgentStatus::Idle) => {
                        debug!(
                            "✅ [cleanup] 状态检查通过: lookup_key={}, user_id={}, 状态=Idle",
                            project_id,
                            user_id.as_deref().unwrap_or("None")
                        );
                        true
                    }
                    None => {
                        // 🆕 修复3: 状态未知，检查是否在保护期内（创建时间 < 5分钟）
                        let age = current_time - created_at;
                        if age.num_seconds() < 300 {
                            // 5 分钟保护期
                            debug!(
                                "⏸️ [cleanup] 状态为 None 且在保护期内，跳过: lookup_key={}, user_id={}, 创建时长={}秒",
                                project_id,
                                user_id.as_deref().unwrap_or("None"),
                                age.num_seconds()
                            );
                            false
                        } else {
                            debug!(
                                "⚠️ [cleanup] 状态为 None 但已超过保护期，将检查超时: lookup_key={}, user_id={}",
                                project_id,
                                user_id.as_deref().unwrap_or("None")
                            );
                            true
                        }
                    }
                    Some(AgentStatus::Active) => {
                        debug!(
                            "⏸️ [cleanup] 跳过Active状态agent: lookup_key={}, user_id={}",
                            project_id,
                            user_id.as_deref().unwrap_or("None")
                        );
                        active_agents += 1;
                        false
                    }
                    Some(AgentStatus::Terminating) => {
                        debug!(
                            "🔄 [cleanup] 跳过Terminating状态agent: lookup_key={}, user_id={}",
                            project_id,
                            user_id.as_deref().unwrap_or("None")
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
                        // 🆕 修复5: 二次确认 - 通过 gRPC 查询容器内 agent 的真实状态
                        if let Some(ref container_info) = container {
                            let user_id_str = user_id.as_deref().unwrap_or(&project_id);
                            if self
                                .is_container_active(container_info, user_id_str, &project_id)
                                .await
                            {
                                info!(
                                    "🛡️ [cleanup] 容器内 agent 正在执行任务，跳过清理: lookup_key={}, user_id={}",
                                    project_id,
                                    user_id.as_deref().unwrap_or("None")
                                );
                                active_agents += 1;
                                continue; // 跳过此容器
                            }
                        }

                        let idle_duration_secs = idle_duration.num_seconds();
                        let age_secs = age.num_seconds();

                        // 🆕 修复4: 改进日志格式，显示 lookup_key、user_id、service_type
                        info!(
                            "🎯 [cleanup] 发现待清理agent: lookup_key={}, user_id={}, service_type={:?}, 状态={:?}, 最后活动={}, 闲置时长={}秒, 创建时长={}秒, 创建时间={}",
                            project_id,
                            user_id.as_deref().unwrap_or("None"),
                            service_type,
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
                            "⏰ [cleanup] Agent未超时或在保护期，跳过清理: lookup_key={}, user_id={}",
                            project_id,
                            user_id.as_deref().unwrap_or("None")
                        );
                    }
                }
            }
        }

        info!(
            "📊 [cleanup] 扫描完成: 总数={}, 待清理={}, 保护期内={}, 活跃状态={}, 未超时={}",
            self.state.projects.len(),
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

        // 清理完成后的统计（使用 DuckDB 存储）
        // 注意：session 数据已合并到 projects 表，active_sessions 等于 remaining_agents
        let remaining_agents = self.state.projects.len();
        let active_sessions = remaining_agents;

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

        // 🔧 使用预计算的容器模式列表
        if self.container_patterns.is_empty() {
            warn!("⚠️ 没有启用的服务，跳过孤立容器清理");
            return 0;
        }

        info!(
            "🔍 [cleanup] 检查孤立容器，服务模式: {:?}",
            self.container_patterns
        );

        let mut total_cleaned = 0;

        // 🔧 遍历所有服务类型的容器模式
        for pattern in &self.container_patterns {
            info!("🔍 [cleanup] 扫描模式: {}", pattern);

            // 从模式推断服务类型
            let service_type = Self::infer_service_type_from_pattern(pattern);

            if service_type.is_none() {
                warn!("⚠️ [cleanup] 无法识别的容器模式: {}", pattern);
                continue;
            }

            let service_type = match service_type {
                Some(st) => st,
                None => continue, // 已在上面检查，不会到达这里
            };

            // 🚀 优化1: 使用更快的容器列表查询，只获取基本信息
            let containers = match docker_manager.list_containers_with_pattern(pattern).await {
                Ok(containers) => containers,
                Err(e) => {
                    warn!("列出容器失败（模式: {}），跳过: {}", pattern, e);
                    continue;
                }
            };

            if containers.is_empty() {
                debug!("✅ [cleanup] 未发现任何 {} 容器", pattern);
                continue;
            }

            info!(
                "🔍 [cleanup] 找到 {} 个匹配模式 '{}' 的容器",
                containers.len(),
                pattern
            );

            // 🚀 优化2: 批量处理，减少Docker API调用次数
            let mut orphaned_containers: Vec<OrphanedContainerInfo> = Vec::new();
            let mut protected_count = 0;

            // 快速筛选孤立容器（不进行详细的Docker查询）
            for container in containers {
                if let Some(names) = &container.names {
                    for name in names {
                        let clean_name = name.trim_start_matches('/');

                        // 🔧 动态解析前缀（兼容多种服务类型）
                        if let Some(project_id) =
                            Self::extract_project_id_from_container_name(clean_name)
                        {
                            // 检查 DuckDB 存储中是否有对应记录
                            if !self.state.contains_project(&project_id) {
                                // 🚀 优化3: 使用容器的创建时间而不是查询详细信息
                                let created_time = container
                                    .created
                                    .and_then(|ts| DateTime::from_timestamp(ts, 0));

                                if let Some(created_at) = created_time {
                                    if self.should_skip_cleanup_due_to_protection(
                                        created_at,
                                        &project_id,
                                    ) {
                                        protected_count += 1;
                                        debug!("🛡️ [cleanup] 容器在保护期内，跳过: {}", clean_name);
                                        break;
                                    }
                                }

                                info!(
                                    "🗑️ [cleanup] 发现孤立容器: {} (id={}, type={:?})",
                                    clean_name, project_id, service_type
                                );

                                // 👈 使用新的结构体
                                orphaned_containers.push(OrphanedContainerInfo {
                                    id: project_id.clone(),
                                    container_name: clean_name.to_string(),
                                    service_type: service_type.clone(),
                                    created_at: created_time,
                                });
                                break; // 找到匹配的名称后跳出内层循环
                            }
                        } else {
                            warn!("⚠️ [cleanup] 无法解析容器名称: {}", clean_name);
                        }
                    }
                }
            }

            if orphaned_containers.is_empty() {
                debug!("✅ [cleanup] 未发现孤立容器（模式: {}）", pattern);
                continue;
            }

            info!(
                "🗑️ [cleanup] 发现 {} 个孤立容器（模式: {}），开始清理",
                orphaned_containers.len(),
                pattern
            );

            // 🚀 优化4: 限制单次清理数量，避免长时间阻塞
            let max_cleanup_per_round = 5; // 减少到5个，避免阻塞
            let total_orphaned = orphaned_containers.len();
            let containers_to_clean = orphaned_containers
                .into_iter()
                .take(max_cleanup_per_round)
                .collect::<Vec<_>>();

            if containers_to_clean.len() < total_orphaned {
                info!(
                    "🔒 [cleanup] 限制单次清理数量为 {}，剩余容器将在下次清理",
                    max_cleanup_per_round
                );
            }

            // 🚀 优化5: 并行清理容器，提高效率
            let mut cleanup_tasks = Vec::new();

            for container_info in containers_to_clean {
                let docker_manager_clone = docker_manager.clone();
                let state_clone = self.state.clone();

                let task = tokio::spawn(async move {
                    let cleanup_timeout = Duration::from_secs(30);
                    match timeout(
                        cleanup_timeout,
                        Self::cleanup_single_orphaned_container(
                            &docker_manager_clone,
                            &state_clone,
                            &container_info, // 👈 传递完整信息
                        ),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {
                            info!(
                                "✅ [cleanup] 成功清理孤立容器: {}",
                                container_info.container_name
                            );
                            true
                        }
                        Ok(Err(e)) => {
                            warn!(
                                "❌ [cleanup] 清理孤立容器失败: {} - {}",
                                container_info.container_name, e
                            );
                            false
                        }
                        Err(_) => {
                            warn!(
                                "⏰ [cleanup] 清理孤立容器超时: {} (超时时间: {}秒)",
                                container_info.container_name,
                                cleanup_timeout.as_secs()
                            );
                            false
                        }
                    }
                });

                cleanup_tasks.push(task);
            }

            // 等待所有清理任务完成
            for task in cleanup_tasks {
                if let Ok(success) = task.await
                    && success
                {
                    total_cleaned += 1;
                }
            }

            if protected_count > 0 {
                info!(
                    "🛡️ [cleanup] 保护期内容器数量（模式: {}）: {}",
                    pattern, protected_count
                );
            }
        }

        if total_cleaned > 0 {
            info!(
                "🧹 [cleanup] 孤立容器检查完成: 共清理 {} 个容器（所有服务类型）",
                total_cleaned
            );
        } else {
            debug!("✅ [cleanup] 未发现需要清理的孤立容器");
        }

        total_cleaned
    }

    /// 从容器名称中提取 project_id 或 user_id
    ///
    /// 支持多种服务类型的容器命名：
    /// - rcoder-agent-{project_id}
    /// - agent-runner-{project_id}
    /// - computer-agent-runner-{user_id}
    ///
    /// # Arguments
    ///
    /// * `container_name` - 容器名称（已去除前导斜杠）
    ///
    /// # Returns
    ///
    /// 如果成功解析返回 `Some(project_id)`，否则返回 `None`
    fn extract_project_id_from_container_name(container_name: &str) -> Option<String> {
        // 移除前导斜杠（如果还有的话）
        let name = container_name.trim_start_matches('/');

        // 尝试匹配已知的服务前缀
        for service_type in [
            shared_types::ServiceType::RCoder,
            shared_types::ServiceType::ComputerAgentRunner,
        ] {
            let prefix = format!("{}-", service_type.container_prefix());
            if let Some(project_id) = name.strip_prefix(&prefix) {
                return Some(project_id.to_string());
            }
        }

        // 如果都不匹配，返回 None
        None
    }

    /// 从容器名称中提取标识符及其服务类型
    ///
    /// **已废弃**: 使用 `infer_service_type_from_pattern` + `extract_project_id_from_container_name` 替代
    ///
    /// 支持多种服务类型的容器命名：
    /// - rcoder-agent-{project_id} -> (project_id, ServiceType::RCoder)
    /// - computer-agent-runner-{user_id} -> (user_id, ServiceType::ComputerAgentRunner)
    ///
    /// # Arguments
    ///
    /// * `container_name` - 容器名称（已去除前导斜杠）
    ///
    /// # Returns
    ///
    /// 如果成功解析返回 `Some((id, ServiceType))`，否则返回 `None`
    #[deprecated(note = "使用 infer_service_type_from_pattern 和 OrphanedContainerInfo 替代")]
    #[allow(dead_code)]
    fn extract_id_and_service_type_from_container_name(
        container_name: &str,
    ) -> Option<(String, shared_types::ServiceType)> {
        // 移除前导斜杠（如果还有的话）
        let name = container_name.trim_start_matches('/');

        // 尝试匹配已知的服务前缀
        for service_type in [
            shared_types::ServiceType::RCoder,
            shared_types::ServiceType::ComputerAgentRunner,
        ] {
            let prefix = format!("{}-", service_type.container_prefix());
            if let Some(id) = name.strip_prefix(&prefix) {
                return Some((id.to_string(), service_type));
            }
        }

        // 如果都不匹配，返回 None
        None
    }

    /// 清理单个孤立容器
    ///
    /// 使用统一的运行时清理策略
    /// 对于 ComputerAgentRunner 容器，同时清理 Pingora VNC 后端映射
    ///
    /// # 参数
    /// * `docker_manager` - Docker 管理器
    /// * `state` - 应用状态
    /// * `info` - 孤立容器的完整信息（包含 ServiceType）
    async fn cleanup_single_orphaned_container(
        docker_manager: &Arc<docker_manager::DockerManager>,
        state: &Arc<AppState>,
        info: &OrphanedContainerInfo,
    ) -> Result<()> {
        info!(
            "🔥 开始清理孤立容器: {} (id={}, type={:?})",
            info.container_name, info.id, info.service_type
        );

        // 查找容器信息
        let container_info = match docker_manager
            .find_container_by_identifier(&info.container_name)
            .await
        {
            Some(info) => info,
            None => {
                info!("📭 容器不存在，无需清理: {}", info.container_name);
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

        // 🆕 对于 ComputerAgentRunner 容器，清理 Pingora VNC 后端映射
        // 👍 直接使用已知的 service_type，无需重新解析
        if info.service_type == shared_types::ServiceType::ComputerAgentRunner {
            if let Some(ref pingora_service) = state.pingora_service {
                // 对于 ComputerAgentRunner，info.id 是 user_id
                if let Some(removed_ip) = pingora_service.remove_vnc_backend(&info.id) {
                    info!(
                        "🧹 [VNC] 已清理 VNC 后端映射: user_id={} -> {}",
                        info.id, removed_ip
                    );
                } else {
                    debug!("📭 [VNC] VNC 后端映射不存在，跳过清理: user_id={}", info.id);
                }
            }
        }

        info!("✅ 容器清理成功: {}", info.container_name);
        Ok(())
    }

    /// 基于RAII的简化清理方法
    /// 先销毁Docker容器，再从存储中移除agent，AgentLifecycleGuard会自动清理其他资源
    async fn cleanup_agent_raii(&self, project_id: &str) -> Result<()> {
        info!("🚀 [cleanup_agent_raii] 开始RAII清理agent: {}", project_id);

        // 为整个清理过程添加超时机制，防止无限阻塞
        let cleanup_result = tokio::time::timeout(
            Duration::from_secs(60), // 60秒总超时
            async {
                info!(
                    "📋 [cleanup_agent_raii] 步骤1: 开始销毁Docker容器: {}",
                    project_id
                );

                // 首先销毁Docker容器（如果存在）
                if let Err(e) = self.destroy_docker_container(project_id).await {
                    warn!(
                        "⚠️ [cleanup_agent_raii] 销毁Docker容器失败: {} - {}",
                        project_id, e
                    );
                } else {
                    info!("✅ [cleanup_agent_raii] Docker容器销毁完成: {}", project_id);
                }

                info!(
                    "📋 [cleanup_agent_raii] 步骤2: 开始清理存储映射: {}",
                    project_id
                );

                // 🔒 使用 DuckDB 存储进行清理
                info!(
                    "🔍 [cleanup_agent_raii] 尝试从存储中移除agent: {}",
                    project_id
                );

                // 获取 session_id 用于后续清理（如果有的话）
                let session_id_to_remove =
                    if let Some(agent_info) = self.state.get_project(project_id) {
                        info!(
                            "✅ [cleanup_agent_raii] 找到agent，提取session_id: {}",
                            project_id
                        );
                        let session_id = agent_info.session_id().map(|s| s.to_string());
                        info!(
                            "📝 [cleanup_agent_raii] 提取session_id: {:?}, project_id: {}",
                            session_id, project_id
                        );
                        session_id
                    } else {
                        info!(
                            "📭 [cleanup_agent_raii] Agent不存在于存储中，无需清理: {}",
                            project_id
                        );
                        return Ok(());
                    };

                // 从 DuckDB 存储中移除项目（这会同时清理 session 关联）
                if let Some(removed_info) = self.state.remove_project(project_id) {
                    info!(
                        "✅ [cleanup_agent_raii] 成功从存储中移除agent: {}, session_id={:?}",
                        project_id,
                        removed_info.session_id()
                    );
                } else {
                    info!(
                        "📭 [cleanup_agent_raii] Agent已被其他线程移除: {}",
                        project_id
                    );
                }

                // 注意：使用 DuckDB 后，session 数据已合并到 projects 表
                // 删除项目时，相关的 session 映射会自动清理，无需手动清理 sessions 和 session_to_container_id
                if let Some(ref session_id) = session_id_to_remove {
                    info!(
                        "📝 [cleanup_agent_raii] Session {} 的映射已随项目一起清理",
                        session_id
                    );
                }

                info!("✅ [cleanup_agent_raii] 存储清理完成: {}", project_id);
                info!("🎯 [cleanup_agent_raii] 所有清理步骤完成: {}", project_id);
                Ok::<(), anyhow::Error>(())
            },
        )
        .await;

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
    /// 🚀 重构：使用 DockerManager 的高级 API，移除底层查找和解析逻辑
    /// 🆕 支持 ComputerAgentRunner 容器的 VNC 后端清理
    ///
    /// # 参数
    /// * `lookup_key` - 容器查找键（RCoder 模式为 project_id，ComputerAgentRunner 模式为 user_id）
    async fn destroy_docker_container(&self, lookup_key: &str) -> Result<()> {
        info!("🔥 [cleanup] 开始销毁Docker容器: lookup_key={}", lookup_key);

        // 使用全局 DockerManager
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| anyhow::anyhow!("获取全局 DockerManager 失败: {}", e))?;

        // 获取 ServiceType，如果未找到则默认为 RCoder（使用 DuckDB 存储）
        let service_type = if let Some(info) = self.state.get_project(lookup_key) {
            info.service_type()
                .unwrap_or(shared_types::ServiceType::RCoder)
        } else {
            shared_types::ServiceType::RCoder
        };

        // 1. 查找容器 (使用新 API)
        if let Some(container_info) = docker_manager
            .find_agent_container(lookup_key, &service_type)
            .await
        {
            info!(
                "🎯 [cleanup] 找到容器: lookup_key={}, container_id={}",
                lookup_key, container_info.container_id
            );

            // 2. 获取连接信息 (使用新 API)
            match docker_manager
                .get_container_connection_info(&container_info)
                .await
            {
                Ok(ip_addr) => {
                    // 3. 清理 gRPC 连接池
                    if let Some(ip) = ip_addr {
                        let grpc_addr = format!("{}:{}", ip, shared_types::GRPC_DEFAULT_PORT);
                        info!("🔌 [gRPC Cleanup] 准备从连接池移除: {}", grpc_addr);
                        self.state.grpc_pool.remove(&grpc_addr);
                    } else {
                        warn!(
                            "⚠️ [gRPC Cleanup] 无法获取容器 IP，无法从连接池中清理: {}",
                            lookup_key
                        );
                    }
                }
                Err(e) => {
                    warn!("⚠️ [cleanup] 获取容器连接信息失败: {} - {}", lookup_key, e);
                }
            }

            // 4. 执行物理销毁 (使用统一接口)
            docker_manager::container_stop::runtime_cleanup_container(
                &docker_manager,
                &container_info.container_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("停止容器失败: {}", e))?;

            // 🆕 5. 对于 ComputerAgentRunner 容器，清理 Pingora VNC 后端映射
            // lookup_key 在 ComputerAgentRunner 模式下就是 user_id
            if service_type == shared_types::ServiceType::ComputerAgentRunner {
                if let Some(ref pingora_service) = self.state.pingora_service {
                    if let Some(removed_ip) = pingora_service.remove_vnc_backend(lookup_key) {
                        info!(
                            "🧹 [VNC] 已清理 VNC 后端映射: user_id={} -> {}",
                            lookup_key, removed_ip
                        );
                    } else {
                        debug!(
                            "📭 [VNC] VNC 后端映射不存在，跳过清理: user_id={}",
                            lookup_key
                        );
                    }
                }
            }

            info!("✅ [cleanup] Docker容器销毁完成: lookup_key={}", lookup_key);
        } else {
            info!("📭 [cleanup] Docker容器不存在: lookup_key={}", lookup_key);
        }

        Ok(())
    }

    /// 🆕 查询容器内 agent 的真实状态
    ///
    /// 返回 true 表示容器内 agent 正在执行任务（不应清理）
    async fn is_container_active(
        &self,
        container_info: &shared_types::ContainerBasicInfo,
        user_id: &str,
        project_id: &str,
    ) -> bool {
        let grpc_addr = format!(
            "{}:{}",
            container_info.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        );

        // 使用短超时（3秒），避免阻塞清理任务
        let timeout_duration = Duration::from_secs(3);

        match timeout(
            timeout_duration,
            self.query_container_status(&grpc_addr, user_id, project_id),
        )
        .await
        {
            Ok(Ok(is_active)) => {
                if is_active {
                    info!(
                        "🛡️ [cleanup] 容器内 agent 正在执行任务: user_id={}, project_id={}",
                        user_id, project_id
                    );
                }
                is_active
            }
            Ok(Err(e)) => {
                // gRPC 查询失败，可能容器已经不健康，允许清理
                debug!(
                    "⚠️ [cleanup] gRPC 查询失败，允许清理: {} - {}",
                    project_id, e
                );
                false
            }
            Err(_) => {
                // 超时，可能容器已经不健康，允许清理
                debug!("⚠️ [cleanup] gRPC 查询超时，允许清理: {}", project_id);
                false
            }
        }
    }

    /// 🆕 调用 gRPC GetContainerStatus
    async fn query_container_status(
        &self,
        grpc_addr: &str,
        user_id: &str,
        project_id: &str,
    ) -> Result<bool> {
        let mut client = self.state.grpc_pool.get_client(grpc_addr).await?;

        let request = tonic::Request::new(GetContainerStatusRequest {
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
        });

        let response = client.get_container_status(request).await?;
        let status = response.into_inner();

        debug!(
            "📊 [cleanup] gRPC 查询容器状态: user_id={}, is_active={}, active_tasks={}, status={}",
            user_id, status.is_active, status.active_tasks, status.status
        );

        Ok(status.is_active || status.active_tasks > 0)
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
}

/// 启动清理任务 - 普通异步版本
///
/// 清理任务只操作 Send 数据结构，可以在普通异步线程中运行
pub fn start_cleanup_task(
    config: CleanupConfig,
    state: Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    // 🔧 从 AppState 获取多镜像配置
    let multi_image_config = state
        .config
        .docker_config
        .as_ref()
        .map(|dc| dc.get_multi_image_config())
        .unwrap_or_else(shared_types::create_default_multi_image_config);

    let mut cleaner = AgentCleaner::new(config, state, multi_image_config);

    tokio::task::spawn(async move {
        cleaner.run().await;
    })
}

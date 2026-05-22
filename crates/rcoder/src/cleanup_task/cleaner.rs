//! 主清理器
//!
//! 协调各模块完成清理任务

use anyhow::Result;
use chrono::Utc;
use shared_types::ServiceType;
use std::sync::Arc;
use tokio::time::interval;
use tracing::{debug, info, warn};

/// 主清理器（协调各模块）
pub struct AgentCleaner {
    config: super::config::CleanupConfig,
    stats: super::config::CleanupStats,
    state: Arc<crate::router::AppState>,

    // 策略
    rcoder_strategy: super::strategies::rcoder::RCoderStrategy,
    computer_runner_strategy: super::strategies::computer_runner::ComputerRunnerStrategy,

    // 组件
    container_destroyer: super::container::ContainerDestroyer,
    agent_scanner: super::agent::AgentScanner,
    log_cleaner: super::logs::LogCleaner,
}

impl AgentCleaner {
    pub fn new(
        config: super::config::CleanupConfig,
        state: Arc<crate::router::AppState>,
        docker_manager: Arc<docker_manager::DockerManager>,
        pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    ) -> Self {
        let config_clone = config.clone();
        let state_clone = state.clone();
        let grpc_pool = state.grpc_pool.clone();

        // 创建日志清理器（使用配置）
        let log_cleaner = super::logs::LogCleaner::new(
            config.log_dir.clone(),
            config.log_retention_duration.as_secs() / 24 / 60 / 60,
        );

        Self {
            config,
            stats: super::config::CleanupStats::default(),
            state: state_clone,
            rcoder_strategy: super::strategies::rcoder::RCoderStrategy,
            computer_runner_strategy: super::strategies::computer_runner::ComputerRunnerStrategy,
            container_destroyer: super::container::ContainerDestroyer::new(
                docker_manager.clone(),
                grpc_pool,
                pingora_service,
            ),
            agent_scanner: {
                use crate::cleanup_task::agent::AgentScanner;
                AgentScanner::new(state.clone(), config_clone)
            },
            log_cleaner,
        }
    }

    /// 执行一次清理
    pub async fn cleanup_once(&mut self) -> Result<super::config::CleanupStats> {
        let start_time = std::time::Instant::now();

        // 重置本次清理的统计
        let mut current_stats = super::config::CleanupStats::default();

        // 1. 清理过期日志文件
        match self.log_cleaner.cleanup_once().await {
            Ok(log_stats) => {
                if log_stats.files_deleted > 0 || log_stats.failed_deletions > 0 {
                    info!(
                        "🗑️ [cleaner] Cleanup completed: {}",
                        log_stats.summary()
                    );
                }
            }
            Err(e) => {
                warn!("[cleaner] Cleanup failed: {}", e);
            }
        }

        // 2. 扫描需要清理的 agent
        let idle_agents = self.agent_scanner.scan_idle_agents().await?;
        info!("[cleaner] Found {} idle agents to clean", idle_agents.len());

        // 3. 清理每个 agent
        // 记录已销毁的容器名称，避免共享容器被重复销毁（ComputerAgentRunner 场景）
        let mut destroyed_containers: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for project_id in idle_agents {
            // 检查该项目的容器是否已被本轮销毁（共享容器场景）
            let container_name: Option<String> = self
                .state
                .get_project(&project_id)
                .and_then(|info| info.container().map(|c| c.container_name.clone()));

            if let Some(ref name) = container_name {
                if destroyed_containers.contains(name) {
                    // 容器已被销毁，只需删除项目记录
                    self.state.remove_project(&project_id);
                    current_stats.total_cleaned += 1;
                    current_stats.success_cleaned += 1;
                    info!(
                        "[cleaner] Container already destroyed, removed project record only: project_id={}",
                        project_id
                    );
                    continue;
                }
            }

            current_stats.total_cleaned += 1;

            match self.cleanup_agent(&project_id).await {
                Ok(destroyed) => {
                    current_stats.success_cleaned += 1;
                    if destroyed {
                        current_stats.containers_destroyed += 1;
                        // 记录已销毁的容器名称，跳过后续同容器的销毁
                        if let Some(name) = container_name {
                            destroyed_containers.insert(name);
                        }
                    }
                    info!("[cleaner] Agent cleanupsucceeded: {}", project_id);
                }
                Err(e) => {
                    current_stats.failed_cleaned += 1;
                    warn!("[cleaner] Agent cleanupfailed: {} - {}", project_id, e);
                }
            }
        }

        // 4. 更新累计统计
        current_stats.last_cleanup = Some(Utc::now());
        self.stats.total_cleaned += current_stats.total_cleaned;
        self.stats.success_cleaned += current_stats.success_cleaned;
        self.stats.failed_cleaned += current_stats.failed_cleaned;
        self.stats.containers_destroyed += current_stats.containers_destroyed;
        self.stats.last_cleanup = current_stats.last_cleanup;

        let duration = start_time.elapsed();
        info!(
            "✅ [cleaner] Cleanup completed, duration: {:.2}s, this run: {}",
            duration.as_secs_f64(),
            current_stats.summary()
        );
        info!("[cleaner] stats: {}", self.stats.summary());

        Ok(current_stats)
    }

    /// 清理单个 agent
    /// 返回 Ok(true) 表示销毁了容器，Ok(false) 表示只删除了记录
    async fn cleanup_agent(&self, project_id: &str) -> Result<bool> {
        info!("[cleaner] startingcleanup agent: {}", project_id);

        // 1. 获取项目信息
        let agent_info = self
            .state
            .get_project(project_id)
            .ok_or_else(|| anyhow::anyhow!("Agent does not exist: {}", project_id))?;

        let service_type = agent_info.service_type().unwrap_or(ServiceType::RCoder);

        // 2. 选择策略
        let strategy: &dyn super::strategies::CleanupStrategy = match service_type {
            ServiceType::RCoder => &self.rcoder_strategy,
            ServiceType::ComputerAgentRunner => &self.computer_runner_strategy,
        };

        // 3. 检查是否需要销毁容器，并获取销毁原因
        let context = super::strategies::CleanupContext {
            state: self.state.clone(),
            config: self.config.clone(),
        };

        let destroy_reason = strategy
            .should_destroy_container(project_id, &context)
            .await?;

        // 4. 如果需要销毁容器
        let mut container_destroyed = false;
        if let Some(reason) = destroy_reason {
            if let Some(container_info) = agent_info.container() {
                let project_info = super::strategies::ProjectInfo {
                    project_id: agent_info.project_id().to_string(),
                    user_id: agent_info.user_id().map(|s| s.to_string()),
                    pod_id: agent_info.pod_id().map(|s| s.to_string()),
                    last_activity: agent_info.last_activity(),
                };

                let container_identifier = strategy.get_container_identifier(&project_info)?;

                // 🔧 使用容器名称而不是 container_id 来销毁容器
                // 容器名称更稳定，不会因为容器重启而改变
                // Docker API 的 remove_container 既接受 ID 也接受名称
                self.container_destroyer
                    .destroy_with_reason(
                        &container_info.container_name,
                        &service_type,
                        &container_identifier,
                        &reason,
                    )
                    .await?;

                container_destroyed = true;
            }
        }

        // 5. 从存储中移除项目记录（始终执行）
        self.state.remove_project(project_id);
        info!(
            "[cleaner] already removed project: project_id={}",
            project_id
        );

        Ok(container_destroyed)
    }

    /// 运行清理任务（定时）
    pub async fn run(&mut self) {
        info!("[cleaner] cleanup task already started");

        let mut interval = interval(self.config.cleanup_interval);
        let mut memory_log_counter = 0u32;
        const MEMORY_LOG_INTERVAL: u32 = 12; // 每 12 次清理（大约 1 小时）输出一次内存日志

        loop {
            interval.tick().await;

            match self.cleanup_once().await {
                Ok(_) => debug!("[cleaner] Cleanup completed"),
                Err(e) => warn!("[cleaner] Cleanup failed: {}", e),
            }

            // 定期输出 DuckDB 内存使用统计
            memory_log_counter += 1;
            if memory_log_counter >= MEMORY_LOG_INTERVAL {
                memory_log_counter = 0;
                if let Ok(stats) = self.state.projects.get_memory_stats() {
                    debug!("[cleaner] DuckDB Memory Stats:\n{}", stats);
                }
            }
        }
    }
}

//! 定期清理闲置agent的任务
//!
//! 提供定时扫描和清理闲置agent的功能

use std::time::Duration;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::model::{AgentStatus, AgentType};
use crate::proxy_agent::{PROJECT_AND_AGENT_INFO_MAP, agent_service::AcpAgentService};

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 闲置超时时间（默认30分钟）
    pub idle_timeout: Duration,
    /// 清理检查间隔（默认5分钟）
    pub cleanup_interval: Duration,
    /// 强制终止超时（默认1分钟）
    pub force_terminate_timeout: Duration,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30 * 60),
            cleanup_interval: Duration::from_secs(5 * 60),
            force_terminate_timeout: Duration::from_secs(60),
        }
    }
}

/// 清理任务控制命令
#[derive(Debug, Clone)]
pub enum CleanupCommand {
    /// 启动清理任务
    Start(CleanupConfig),
    /// 停止清理任务
    Stop,
    /// 立即执行一次清理
    CleanupNow,
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
    /// 最后清理时间
    pub last_cleanup: Option<DateTime<Utc>>,
}

/// Agent清理器
pub struct AgentCleaner {
    config: CleanupConfig,
    stats: CleanupStats,
    running: bool,
}

impl AgentCleaner {
    /// 创建新的清理器
    pub fn new(config: CleanupConfig) -> Self {
        Self {
            config,
            stats: CleanupStats::default(),
            running: false,
        }
    }

    /// 检查agent是否闲置超时
    fn is_agent_idle_timeout(&self, last_activity: DateTime<Utc>, current_time: DateTime<Utc>) -> bool {
        let duration = current_time.signed_duration_since(last_activity);
        duration.num_seconds() > 0 && duration.num_seconds() as u64 > self.config.idle_timeout.as_secs()
    }

    /// 执行一次清理操作
    async fn cleanup_idle_agents(&mut self) -> CleanupStats {
        let current_time = Utc::now();
        let mut cleaned_count = 0;
        let mut success_count = 0;
        let mut failed_count = 0;

        info!("开始清理闲置agent，当前时间: {}", current_time);

        // 收集需要清理的agent ID
        let mut agents_to_remove = Vec::new();

        for entry in PROJECT_AND_AGENT_INFO_MAP.iter() {
            let project_id = entry.key();
            let agent_info = entry.value();

            // 只清理Idle状态的agent，避免中断正在执行的任务
            if agent_info.status == AgentStatus::Idle {
                if self.is_agent_idle_timeout(agent_info.last_activity, current_time) {
                    info!(
                        "发现闲置agent: project_id={}, 状态={}, 最后活动: {}, 闲置时长: {}秒",
                        project_id,
                        format!("{:?}", agent_info.status),
                        agent_info.last_activity,
                        (current_time - agent_info.last_activity).num_seconds()
                    );
                    agents_to_remove.push(project_id.clone());
                }
            }
        }

        // 执行清理
        for project_id in agents_to_remove {
            match self.cleanup_agent(&project_id).await {
                Ok(_) => {
                    success_count += 1;
                    info!("成功清理agent: {}", project_id);
                }
                Err(e) => {
                    failed_count += 1;
                    error!("清理agent失败: {} - {}", project_id, e);
                }
            }
            cleaned_count += 1;
        }

        // 更新统计信息
        self.stats.total_cleaned += cleaned_count;
        self.stats.success_cleaned += success_count;
        self.stats.failed_cleaned += failed_count;
        self.stats.last_cleanup = Some(current_time);

        info!(
            "清理完成: 总共={}, 成功={}, 失败={}",
            cleaned_count, success_count, failed_count
        );

        CleanupStats {
            total_cleaned: cleaned_count,
            success_cleaned: success_count,
            failed_cleaned: failed_count,
            last_cleanup: Some(current_time),
        }
    }

    /// 清理单个agent
    async fn cleanup_agent(&self, project_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!("开始清理agent: {}", project_id);

        // 获取agent信息
        if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
            // 设置状态为Terminating
            if let Some(mut agent_info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(project_id) {
                agent_info.status = AgentStatus::Terminating;
                agent_info.is_stopping = true;
                agent_info.last_activity = Utc::now();
                debug!("设置agent状态为Terminating: {}", project_id);
            }

            // 使用AcpAgentService trait的停止方法
            let agent_type = agent_info.model_provider.as_ref()
                .map(|mp| AgentType::from_model_provider(Some(mp)))
                .unwrap_or_default();

            info!("使用[{}] agent的协作停止方法清理: {}", agent_type.agent_type_name(), project_id);

            // 先发送取消信号，让任务优雅退出
            agent_type.cancel_agent_service(&agent_info);

            // 等待一段时间让任务优雅退出
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

            // 如果还没有停止，则强制停止
            if let Err(e) = agent_type.stop_agent_service(&agent_info).await {
                error!("停止agent服务失败: {} - {}", project_id, e);
                // 即使停止失败，也要继续清理流程
            }

            // 从map中移除agent信息，触发Drop
            let removed = PROJECT_AND_AGENT_INFO_MAP.remove(project_id);
            if removed.is_some() {
                info!("Agent已从map中移除: {}", project_id);
            }
        } else {
            warn!("Agent不存在于map中: {}", project_id);
        }

        Ok(())
    }

    /// 运行清理任务
    pub async fn run(&mut self, mut command_rx: mpsc::Receiver<CleanupCommand>) {
        info!("Agent清理任务已启动");

        loop {
            tokio::select! {
                // 接收命令
                command = command_rx.recv() => {
                    match command {
                        Some(CleanupCommand::Start(config)) => {
                            self.config = config;
                            self.running = true;
                            info!("清理任务已启动，配置: {:?}", self.config);
                        }
                        Some(CleanupCommand::Stop) => {
                            self.running = false;
                            info!("清理任务已停止");
                            break;
                        }
                        Some(CleanupCommand::CleanupNow) => {
                            info!("立即执行清理");
                            let stats = self.cleanup_idle_agents().await;
                            info!("立即清理完成: {:?}", stats);
                        }
                        None => {
                            info!("命令通道已关闭，停止清理任务");
                            break;
                        }
                    }
                }
                // 定期清理
                _ = async {
                    if self.running {
                        tokio::time::sleep(self.config.cleanup_interval).await;
                    } else {
                        // 如果没有运行，等待更长时间
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                } => {
                    if self.running {
                        let stats = self.cleanup_idle_agents().await;
                        debug!("定期清理完成: {:?}", stats);
                    }
                }
            }
        }

        info!("Agent清理任务已退出");
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> &CleanupStats {
        &self.stats
    }
}

/// 启动清理任务
pub fn start_cleanup_task(config: CleanupConfig) -> (mpsc::Sender<CleanupCommand>, tokio::task::JoinHandle<()>) {
    let (command_tx, command_rx) = mpsc::channel(32);

    let mut cleaner = AgentCleaner::new(config);

    let handle = tokio::task::spawn_local(async move {
        cleaner.run(command_rx).await;
    });

    (command_tx, handle)
}
//! 协调票据模式：DevServerManager 作为预览协调器的本地执行器。
//!
//! 与 legacy 路径的差异（不变量来源 spec.md 执行边界）：
//! - registry 键 = preview_key（不是 project_id），登记携带 instance_id/base_path；
//! - 端口来自协调器全局分配（不走本地 PortPool，端口记账在权威存储）；
//! - stop 只按**记录 pid** 组杀，绝不 ps 扫描、绝不误杀他实例；
//! - 身份不匹配（迟到旧操作打到新登记）返回 IdentityMismatch 而非执行。

use std::path::Path;
use std::sync::Arc;

use shared_types::{
    ExecutorLogChunk, ExecutorStartTicket, ExecutorStopOutcome, ExecutorVerifyReport,
    PreviewExecutor, PreviewExecutorError,
};

use super::process;
use super::start::StartingGuard;
use super::support::lock;
use super::types::{DevServerManager, StartedDev};
use crate::error::{AppError, AppResult};

impl DevServerManager {
    /// 票据启动（协调器签发端口与实例身份）。
    pub async fn start_coordinated(&self, ticket: &ExecutorStartTicket) -> AppResult<StartedDev> {
        let project_path = Path::new(&ticket.project_path);
        // workspace.manifest.toml = userapp 域（app-cli 引擎），不进 Custom Page 协调。
        let manifest = project_path.join("workspace.manifest.toml");
        if tokio::fs::try_exists(&manifest).await.unwrap_or(false) {
            return Err(AppError::business(
                "workspace manifest projects are userapp domain, not coordinated previews",
            ));
        }
        // 启动锁（键=preview_key；受理互斥已由权威存储保证，此处防同进程重复执行）
        {
            let mut starting = lock(&self.starting)?;
            if starting.contains(&ticket.preview_key) {
                return Err(AppError::business(
                    "preview instance already starting on this host",
                ));
            }
            starting.insert(ticket.preview_key.clone());
        }
        let _guard = StartingGuard {
            starting: &self.starting,
            project_id: ticket.preview_key.clone(),
        };
        // 幂等：同实例重放返回现有登记；异实例残留为异常态，fail-fast。
        if let Some(p) = lock(&self.processes)?.get(&ticket.preview_key).cloned() {
            if p.instance_id.as_deref() == Some(&ticket.instance_id) {
                return Ok(StartedDev {
                    pid: p.pid,
                    port: p.port,
                });
            }
            return Err(AppError::business(format!(
                "local registration for preview key exists with a different instance; refusing to start (recorded instance: {:?})",
                p.instance_id
            )));
        }

        self.spawn_and_register(
            &ticket.preview_key,
            &ticket.log_key,
            project_path,
            ticket.port,
            ticket.base_path.as_deref(),
            Some(&ticket.instance_id),
        )
        .await
    }

    /// 票据停止：仅登记匹配时按记录 pid 组杀；登记缺失/身份不符不杀任何进程。
    pub async fn stop_coordinated(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> AppResult<ExecutorStopOutcome> {
        let proc = lock(&self.processes)?.remove(preview_key);
        let Some(p) = proc else {
            return Ok(ExecutorStopOutcome::NotRegistered);
        };
        if p.instance_id.as_deref() != Some(instance_id) {
            // 迟到旧操作打到新登记：把新登记放回，绝不误杀。
            lock(&self.processes)?.insert(preview_key.to_string(), p);
            return Ok(ExecutorStopOutcome::IdentityMismatch);
        }
        let killed = self.terminate_pid_group(p.pid).await;
        if !killed {
            // 进程组已不在（先期退出）也按 Stopped 收口——登记已摘除，端口由存储释放。
            tracing::warn!(
                preview_key,
                pid = p.pid,
                "coordinated stop found process already gone"
            );
        }
        super::log::cleanup_temp_logs(&p.log_dir).await;
        Ok(ExecutorStopOutcome::Stopped)
    }

    /// 票据校验：登记匹配 + 进程探活（心跳/恢复判定用）。
    pub async fn verify_coordinated(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> AppResult<ExecutorVerifyReport> {
        let entry = lock(&self.processes)?.get(preview_key).cloned();
        let Some(p) = entry else {
            return Ok(ExecutorVerifyReport {
                identity_match: false,
                alive: false,
                pid: None,
                port: None,
            });
        };
        if p.instance_id.as_deref() != Some(instance_id) {
            return Ok(ExecutorVerifyReport {
                identity_match: false,
                alive: false,
                pid: Some(i64::from(p.pid)),
                port: Some(p.port),
            });
        }
        let alive = process::is_project_alive(
            p.port,
            p.base_path.as_deref(),
            self.config.dev_alive_check_timeout_ms,
        )
        .await;
        Ok(ExecutorVerifyReport {
            identity_match: true,
            alive,
            pid: Some(i64::from(p.pid)),
            port: Some(p.port),
        })
    }
}

/// [`shared_types::PreviewExecutor`] 的 DevServerManager 适配（rcoder 装配时注入协调器）。
pub struct DevServerExecutor {
    manager: Arc<DevServerManager>,
}

impl DevServerExecutor {
    pub fn new(manager: Arc<DevServerManager>) -> Self {
        Self { manager }
    }
}

fn executor_error(error: AppError) -> PreviewExecutorError {
    let message = error.to_string();
    // ViteStartupError::PortInUse 的稳定标记（error_classify 生成文案）。
    if message.contains("--strictPort 不自动换端口") {
        return PreviewExecutorError::PortInUse(message);
    }
    PreviewExecutorError::Failed(message)
}

#[async_trait::async_trait]
impl PreviewExecutor for DevServerExecutor {
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        let started = self
            .manager
            .start_coordinated(ticket)
            .await
            .map_err(executor_error)?;
        Ok((i64::from(started.pid), started.port))
    }

    async fn stop_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        self.manager
            .stop_coordinated(preview_key, instance_id)
            .await
            .map_err(executor_error)
    }

    async fn verify_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        self.manager
            .verify_coordinated(preview_key, instance_id)
            .await
            .map_err(executor_error)
    }

    async fn read_log_local(
        &self,
        log_key: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        let result = self
            .manager
            .read_dev_log(log_key, start_index, log_type)
            .await
            .map_err(executor_error)?;
        Ok(ExecutorLogChunk {
            logs: result
                .logs
                .into_iter()
                .map(|l| shared_types::ExecutorLogLine {
                    line: l.line,
                    content: l.content,
                })
                .collect(),
            total_lines: result.total_lines,
            log_file_name: result.log_file_name,
        })
    }
}

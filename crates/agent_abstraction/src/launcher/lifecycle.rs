//! Agent生命周期管理
//!
//! 基于RAII原则的简洁生命周期管理设计
//!
//! ## 僵尸进程问题解决方案
//!
//! 核心问题：Drop trait 是同步的，无法 await child.wait()
//!
//! 解决方案：
//! 1. **后台回收任务**：立即启动后台任务 wait() 子进程
//! 2. **进程组终止**：使用 nix::kill 发送信号到进程组
//! 3. **三重保障**：tini(容器 PID 1)兜底回收孤儿进程
//!
//! ## 进程组说明
//!
//! 使用 `process-wrap` crate 创建真正的进程组：
//! - 启动时使用 `ProcessGroup::leader()` 创建进程组
//! - 终止时发送 `kill(-pgid, SIGKILL)` 到整个进程组
//! - 能够正确清理子进程及其所有孙进程

#![allow(dead_code)]

use anyhow::Result;
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use agent_client_protocol::schema::v1::SessionId;
use shared_types::{AgentLifecycle, ModelProviderConfig};

use crate::diagnostics::DiagnosticsListener;

/// Agent生命周期守卫
///
/// 遵循RAII原则，当守卫被drop时自动清理agent资源
///
/// ## 僵尸进程避免机制
///
/// 1. **后台回收任务**：构造时立即启动 tokio::spawn 等待子进程
/// 2. **进程组终止**：Drop 时发送信号到进程组（使用 nix::kill）
/// 3. **PID 1 兜底**：tini(容器 PID 1)自动回收所有孤儿进程
///
/// ## 进程组信号
///
/// 在 Unix 上，使用负的进程组 ID 发送信号：
/// - `kill(-pgid, SIGKILL)` 杀死整个进程组
/// - 这会终止子进程及其所有后代（如果子进程创建了真正的进程组）
pub struct AgentLifecycleGuard {
    inner: Arc<AgentLifecycleInner>,
}

impl std::fmt::Debug for AgentLifecycleGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLifecycleGuard")
            .field("project_id", &self.inner.project_id)
            .field("session_id", &self.inner.session_id)
            .field("pgid", &self.inner.pgid)
            .field("stopped", &self.inner.stopped.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl AgentLifecycleGuard {
    /// 启动阶段把转换租约保持到 handshake 成功或真实 child.wait 成功。
    /// 只克隆退出观察通道，避免共享 inner 妨碍 session_id 初始化。
    pub(crate) fn retain_startup_transition(
        &self,
        transition: Arc<crate::session::ExecutionTransitionGuard>,
    ) -> tokio::sync::oneshot::Sender<()> {
        let (completed, completion) = tokio::sync::oneshot::channel();
        let mut exit = self.inner.exit_result.clone();
        let project_id = self.inner.project_id.clone();
        tokio::spawn(async move {
            let _transition = transition;
            if completion.await.is_ok() {
                return;
            }
            // 启动 future 被取消时 lifecycle Drop 负责发信号，这里保留准入锁直到
            // reaper 的实际成功证据。观察通道丢失或 reaper 错误不能授权双实例。
            loop {
                let result = exit.borrow().clone();
                match result {
                    Some(Ok(())) => return,
                    Some(Err(error)) => {
                        tracing::error!(project_id, %error, "startup exit unconfirmed; retaining execution transition");
                        std::future::pending::<()>().await;
                    }
                    None => {}
                }
                if exit.changed().await.is_err() {
                    tracing::error!(
                        project_id,
                        "startup reaper observation closed; retaining execution transition"
                    );
                    std::future::pending::<()>().await;
                }
            }
        });
        completed
    }

    /// handshake 前守卫尚未共享；完成后再记录真实 ACP session 身份。
    pub(crate) fn set_session_id(&mut self, session_id: SessionId) -> Result<()> {
        crate::session::require_session_identity(&session_id)?;
        let inner = Arc::get_mut(&mut self.inner).ok_or_else(|| {
            anyhow::anyhow!("agent lifecycle was shared before session initialization")
        })?;
        inner.session_id = Some(session_id);
        Ok(())
    }
}

pub(crate) struct AgentLifecycleInner {
    pub(crate) project_id: String,
    pub(crate) session_id: Option<SessionId>,
    /// 🔥 进程组 ID（当前实现：使用 child.pid 作为伪进程组）
    ///
    /// 注意：当前实现使用子进程的 PID 作为 PGID。
    /// - 如果子进程通过 setsid() 创建了真正的进程组，kill(-pgid) 会杀死整个进程树
    /// - 如果子进程没有创建进程组，kill(-pgid) 只会杀死子进程本身
    /// - 未来可以使用 process-wrap 库创建真正的进程组
    pub(crate) pgid: u32,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) resources: AgentResources,
    pub(crate) stopped: AtomicBool,
    /// reaper 完成 child.wait() 后发布的结果，多次停止共享同一退出证据。
    pub(crate) exit_result: tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>,
    /// 🔥 共享的 API 密钥管理器引用（用于自动清理）
    pub(crate) shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    /// 🔥 project_id -> service_uuid 映射（用于清理时查找 UUID）
    pub(crate) project_uuid_map: Option<Arc<DashMap<String, String>>>,
    /// 🔥 关联的 service_uuid（用于清理时定位配置）
    pub(crate) service_uuid: Option<String>,
    /// 🔥 P0-2 接线: 进程诊断监听器
    pub(crate) diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
    /// 启动命令（用于构造 ProcessDiagnostics）
    pub(crate) process_command: String,
    /// 启动参数（用于构造 ProcessDiagnostics）
    pub(crate) process_args: Vec<String>,
    /// 工作目录（用于构造 ProcessDiagnostics）
    pub(crate) working_dir: PathBuf,
}

/// Agent资源管理枚举
///
/// ## 后台回收版本
///
/// 存储后台任务句柄，确保子进程被 wait() 回收
pub(crate) enum AgentResources {
    Claude {
        /// stderr 任务句柄
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
        /// 后台回收任务（已启动，会 wait() 子进程）
        _reaper_task: JoinHandle<()>,
    },
}

// 为AgentLifecycleGuard实现AgentLifecycle trait
impl AgentLifecycle for AgentLifecycleGuard {
    fn graceful_stop(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move { AgentLifecycleGuard::graceful_stop(self).await })
    }

    fn wait_for_exit(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move { AgentLifecycleGuard::wait_for_exit(self).await })
    }

    fn cancel(&self) {
        AgentLifecycleGuard::cancel(self);
    }

    fn is_stopped(&self) -> bool {
        AgentLifecycleGuard::is_stopped(self)
    }

    fn cancellation_token(&self) -> &CancellationToken {
        AgentLifecycleGuard::cancellation_token(self)
    }
}

/// 进程退出详情
///
/// 用于标识进程退出的原因，便于选择对应的 i18n 消息
#[derive(Debug, Clone)]
pub enum ExitDetail {
    /// 被 SIGKILL 杀死（通常是 OOM）
    SigKilled,
    /// 发生段错误
    SigSegv,
    /// 被 SIGTERM 终止
    SigTerm,
    /// 其他信号
    Signal(i32),
    /// 特定退出码
    ExitCode(i32),
    /// 未知原因
    Unknown,
}

impl ExitDetail {
    /// 获取对应的 i18n 消息 key
    pub fn i18n_key(&self) -> &str {
        match self {
            ExitDetail::SigKilled => "error.agent_process_sigkilled",
            ExitDetail::SigSegv => "error.agent_process_sigsegv",
            ExitDetail::SigTerm => "error.agent_process_sigterm",
            ExitDetail::Signal(_) => "error.agent_process_abnormal_exit",
            ExitDetail::ExitCode(_) => "error.agent_process_exit_code",
            ExitDetail::Unknown => "error.agent_process_abnormal_exit",
        }
    }

    /// 获取格式化参数（用于带占位符的消息）
    pub fn format_arg(&self) -> Option<String> {
        match self {
            ExitDetail::ExitCode(code) => Some(code.to_string()),
            _ => None,
        }
    }
}

/// 分析进程退出详情
///
/// 根据 exit_code 和 signal 生成 ExitDetail 枚举，用于选择对应的 i18n 消息。
///
/// # Arguments
/// * `exit_code` - 进程退出码
/// * `signal` - 杀死进程的信号（Unix）
///
/// # Returns
/// ExitDetail 枚举
pub(crate) fn analyze_exit_detail(exit_code: Option<i32>, signal: Option<i32>) -> ExitDetail {
    // 优先检查信号（SIGKILL、SIGTERM 等）
    if let Some(sig) = signal {
        match sig {
            9 => return ExitDetail::SigKilled,
            11 => return ExitDetail::SigSegv,
            15 => return ExitDetail::SigTerm,
            _ => return ExitDetail::Signal(sig),
        }
    }

    // 检查退出码
    if let Some(code) = exit_code {
        match code {
            // 128 + 9 = SIGKILL
            137 => return ExitDetail::SigKilled,
            // 128 + 11 = SIGSEGV
            139 => return ExitDetail::SigSegv,
            // 128 + 15 = SIGTERM
            143 => return ExitDetail::SigTerm,
            _ => return ExitDetail::ExitCode(code),
        }
    }

    ExitDetail::Unknown
}

mod shutdown;
mod spawn;

pub use spawn::ClaudeProcessParams;

#[cfg(all(test, unix))]
mod tests;

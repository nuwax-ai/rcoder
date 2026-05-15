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
//! 3. **三重保障**：PID 1 的 process_reaper 模块兜底
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
use process_wrap::tokio::ChildWrapper;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use agent_client_protocol::schema::SessionId;
use shared_types::{AgentLifecycle, ModelProviderConfig};

/// Agent生命周期守卫
///
/// 遵循RAII原则，当守卫被drop时自动清理agent资源
///
/// ## 僵尸进程避免机制
///
/// 1. **后台回收任务**：构造时立即启动 tokio::spawn 等待子进程
/// 2. **进程组终止**：Drop 时发送信号到进程组（使用 nix::kill）
/// 3. **PID 1 兜底**：process_reaper 模块自动回收所有孤儿进程
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

struct AgentLifecycleInner {
    project_id: String,
    session_id: SessionId,
    /// 🔥 进程组 ID（当前实现：使用 child.pid 作为伪进程组）
    ///
    /// 注意：当前实现使用子进程的 PID 作为 PGID。
    /// - 如果子进程通过 setsid() 创建了真正的进程组，kill(-pgid) 会杀死整个进程树
    /// - 如果子进程没有创建进程组，kill(-pgid) 只会杀死子进程本身
    /// - 未来可以使用 process-wrap 库创建真正的进程组
    pgid: u32,
    cancel_token: CancellationToken,
    resources: AgentResources,
    stopped: AtomicBool,
    /// 🔥 共享的 API 密钥管理器引用（用于自动清理）
    shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    /// 🔥 project_id -> service_uuid 映射（用于清理时查找 UUID）
    project_uuid_map: Option<Arc<DashMap<String, String>>>,
    /// 🔥 关联的 service_uuid（用于清理时定位配置）
    service_uuid: Option<String>,
}

/// Agent资源管理枚举
///
/// ## 后台回收版本
///
/// 存储后台任务句柄，确保子进程被 wait() 回收
enum AgentResources {
    Claude {
        /// stderr 任务句柄
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
        /// 后台回收任务（已启动，会 wait() 子进程）
        _reaper_task: JoinHandle<()>,
    },
}

impl AgentLifecycleGuard {
    /// 为Claude Agent创建生命周期守卫（兼容旧代码，默认无密钥管理器）
    ///
    /// # 参数
    ///
    /// * `child_process` - 已启动的子进程（必须是进程组组长）
    /// * `stderr_task` - stderr 读取任务
    /// * `cancel_token` - 取消令牌
    ///
    /// # 僵尸进程避免
    ///
    /// 此函数会立即启动后台任务等待子进程，确保子进程退出时被回收。
    pub fn new_claude(
        project_id: String,
        session_id: SessionId,
        child_process: Box<dyn ChildWrapper>,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self::new_claude_with_key_manager(
            project_id,
            session_id,
            child_process,
            stderr_task,
            cancel_token,
            None, // 默认无密钥管理器
            None, // 默认无 project_uuid_map
            None, // 默认无 service_uuid
        )
    }

    /// 🔥 新增：带异常退出标志的构造函数
    ///
    /// 创建生命周期守卫时传入共享的 `abnormal_exit_flag`，当子进程异常退出时设置此标志。
    /// 这使得 SACP 连接层可以检测到异常退出并发送相应的通知。
    ///
    /// # 参数
    ///
    /// * `abnormal_exit_flag` - 共享的原子布尔标志，子进程异常退出时设置为 true
    ///
    /// # Panics
    ///
    /// 如果子进程 PID 无效（为 0 或 None），此函数会 panic，因为这意味着进程启动失败。
    pub fn new_claude_with_abnormal_flag(
        project_id: String,
        session_id: SessionId,
        child_process: Box<dyn ChildWrapper>,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
        abnormal_exit_flag: Arc<AtomicBool>,
    ) -> Self {
        Self::new_claude_with_key_manager_and_abnormal_flag(
            project_id,
            session_id,
            child_process,
            stderr_task,
            cancel_token,
            None, // 默认无密钥管理器
            None, // 默认无 project_uuid_map
            None, // 默认无 service_uuid
            Some(abnormal_exit_flag),
        )
    }

    /// 🔥 新增：带密钥管理器和异常退出标志的构造函数
    ///
    /// 创建生命周期守卫时传入共享的 API 密钥管理器和 service_uuid，
    /// 当 Agent 停止时（Drop）会自动清理对应的 API 密钥配置。
    ///
    /// # 进程组管理
    ///
    /// 当前实现使用子进程的 PID 作为 PGID：
    /// - `pgid = child_pid`（使用子进程 PID 作为进程组 ID）
    /// - 终止时发送 `kill(-pgid, SIGKILL)` 到进程组
    /// - 使用 `process-wrap` 创建真正的进程组，能正确清理所有孙进程
    ///
    /// # 参数
    ///
    /// * `shared_api_key_manager` - 共享的 DashMap，用于清理 API 密钥配置
    /// * `project_uuid_map` - project_id -> service_uuid 映射，用于查找 UUID
    /// * `service_uuid` - 与此 Agent 关联的 service UUID
    /// * `abnormal_exit_flag` - 共享的原子布尔标志，子进程异常退出时设置为 true
    ///
    /// # Panics
    ///
    /// 如果子进程 PID 无效（为 0 或 None），此函数会 panic，因为这意味着进程启动失败。
    #[allow(clippy::too_many_arguments)]
    pub fn new_claude_with_key_manager(
        project_id: String,
        session_id: SessionId,
        child_process: Box<dyn ChildWrapper>,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
        shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
        project_uuid_map: Option<Arc<DashMap<String, String>>>,
        service_uuid: Option<String>,
    ) -> Self {
        Self::new_claude_with_key_manager_and_abnormal_flag(
            project_id,
            session_id,
            child_process,
            stderr_task,
            cancel_token,
            shared_api_key_manager,
            project_uuid_map,
            service_uuid,
            None, // 默认无异常退出标志
        )
    }

    /// 🔥 完整构造函数：带密钥管理器和异常退出标志
    #[allow(clippy::too_many_arguments)]
    fn new_claude_with_key_manager_and_abnormal_flag(
        project_id: String,
        session_id: SessionId,
        mut child_process: Box<dyn ChildWrapper>,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
        shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
        project_uuid_map: Option<Arc<DashMap<String, String>>>,
        service_uuid: Option<String>,
        abnormal_exit_flag: Option<Arc<AtomicBool>>,
    ) -> Self {
        // 🔥 关键：PID 有效性检查
        // ChildWrapper 的 id() 返回 Option<u32>，当进程已终止或无效时返回 None
        // 如果 PID 无效，这是一个严重的初始化错误，应该 panic
        let pid = child_process.id().unwrap_or_else(|| {
            panic!(
                "[LifecycleGuard] 子进程 PID 无效（None），进程可能已终止: project_id={}",
                project_id
            )
        });

        // 🔥 额外检查：PID 不应该为 0
        // 虽然 id() 返回 Some(0) 理论上可能，但实际上 PID 0 是内核保留的
        if pid == 0 {
            panic!(
                "[LifecycleGuard] 子进程 PID 为 0，这是无效的 PID: project_id={}",
                project_id
            );
        }

        // 🔥 进程组 ID 等于组长进程的 PID
        // process-wrap 的 ProcessGroup 使用 setpgid(0, 0) 创建新进程组，使进程成为组长
        let pgid = pid;
        let project_id_clone = project_id.clone();
        let session_id_str = session_id.0.to_string();

        // 🔥 关键：立即启动后台回收任务
        // 这个任务会等待子进程退出，确保不会产生僵尸进程
        // 当子进程退出时，设置 abnormal_exit_flag 并触发 cancel_token
        // 让 SACP 连接层检测到并发送 SSE 通知
        let cancel_token_for_reaper = cancel_token.clone();
        let abnormal_exit_flag_clone = abnormal_exit_flag.clone();
        let project_id_for_reaper = project_id.clone();
        let reaper_task = tokio::spawn(async move {
            info!(
                "[ProcessReaper] 开始监控 Agent 进程: project_id={}, pid={}, pgid={}",
                project_id_for_reaper, pid, pgid
            );

            // 🔥 优先等待子进程退出，而不是响应取消信号
            // 这确保了即使收到取消信号，也能正确检测进程是否已退出
            let wait_result = child_process.wait().await;

            // 检查是否是外部取消（用户主动 stop）
            let was_cancelled = cancel_token_for_reaper.is_cancelled();

            match wait_result {
                Ok(status) => {
                    // 获取详细的退出信息
                    let exit_code = status.code();
                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal()
                    };
                    #[cfg(not(unix))]
                    let signal: Option<i32> = None;

                    if !status.success() {
                        // 🔥 非零退出码或被信号杀死 = 异常退出
                        if let Some(ref flag) = abnormal_exit_flag_clone {
                            // 只有非用户主动取消时才标记为异常
                            if !was_cancelled {
                                flag.store(true, Ordering::SeqCst);
                            }
                        }
                        warn!(
                            "[ProcessReaper] Agent 进程异常退出: project_id={}, pid={}, pgid={}, exit_code={:?}, signal={:?}, was_cancelled={}",
                            project_id_for_reaper, pid, pgid, exit_code, signal, was_cancelled
                        );
                    } else {
                        info!(
                            "[ProcessReaper] Agent 进程正常退出: project_id={}, pid={}, pgid={}, exit_code={:?}",
                            project_id_for_reaper, pid, pgid, exit_code
                        );
                    }
                }
                Err(e) => {
                    // wait 失败，可能是进程已被其他方式回收
                    if let Some(ref flag) = abnormal_exit_flag_clone
                        && !was_cancelled
                    {
                        flag.store(true, Ordering::SeqCst);
                    }
                    warn!(
                        "[ProcessReaper] Agent 进程 wait() 失败: project_id={}, pid={}, pgid={}, error={}, was_cancelled={}",
                        project_id_for_reaper, pid, pgid, e, was_cancelled
                    );
                }
            }

            // 🔥 关键：触发 cancel_token，通知 SACP 连接层进程已退出
            // 这会让 SACP 连接检测到并发送 SSE 错误通知，然后断开连接
            if !was_cancelled {
                info!(
                    "[ProcessReaper] 触发 cancel_token，通知 SACP 连接断开: project_id={}, pid={}",
                    project_id_for_reaper, pid
                );
                cancel_token_for_reaper.cancel();
            } else {
                debug!(
                    "[ProcessReaper] cancel_token 已被外部取消，跳过: project_id={}, pid={}",
                    project_id_for_reaper, pid
                );
            }
        });

        let resources = AgentResources::Claude {
            stderr_task: Arc::new(Mutex::new(Some(stderr_task))),
            _reaper_task: reaper_task,
        };

        let inner = Arc::new(AgentLifecycleInner {
            project_id: project_id_clone,
            session_id,
            pgid,
            cancel_token,
            resources,
            stopped: AtomicBool::new(false),
            shared_api_key_manager,
            project_uuid_map,
            service_uuid,
        });

        info!(
            "[LifecycleGuard] 创建 Claude Agent 守卫: project_id={}, pgid={}, session_id={}",
            project_id, pgid, session_id_str
        );

        Self { inner }
    }

    /// 优雅停止agent
    ///
    /// 带超时机制（5秒），超时后强制 kill 进程组
    ///
    /// ## 进程组终止
    ///
    /// 使用 `process-wrap` 创建真正的进程组，发送信号到 `-pgid` 会终止：
    /// - 子进程（进程组组长）
    /// - 所有孙进程（同一进程组中的进程）
    pub async fn graceful_stop(&self) -> Result<()> {
        // 🔥 使用原子 CAS 操作确保只执行一次清理
        // compare_exchange 返回 Ok 表示成功将 false 改为 true，即当前线程获得清理权
        // 返回 Err 表示已经被其他地方清理（Drop 或其他 graceful_stop 调用）
        let should_cleanup = self
            .inner
            .stopped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();

        if !should_cleanup {
            debug!("Agent already stopped, skipping graceful stop");
            return Ok(());
        }

        info!(
            "Gracefully stopping Claude agent for project: {}, pgid={}",
            self.inner.project_id, self.inner.pgid
        );

        // 1. 发送取消信号
        self.inner.cancel_token.cancel();

        // 2. 终止进程组
        self.kill_process_group(false).await?;

        info!(
            "Gracefully stopped Claude agent for project: {}",
            self.inner.project_id
        );
        Ok(())
    }

    /// 发送取消信号（非阻塞）
    pub fn cancel(&self) {
        debug!("Sending cancel signal to agent: {}", self.inner.project_id);
        self.inner.cancel_token.cancel();
    }

    /// 检查是否已停止
    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// 获取取消令牌
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.inner.cancel_token
    }

    /// 🔥 终止进程组
    ///
    /// 向 `-pgid` 发送信号，杀死整个进程组
    ///
    /// # Unix 信号语义
    ///
    /// - `kill(pgid, SIGTERM)` - 发送给单个进程
    /// - `kill(-pgid, SIGTERM)` - 发送给整个进程组
    /// - `kill(0, SIGTERM)` - 发送给调用者自己的进程组（危险！）
    ///
    /// # 参数
    ///
    /// * `force` - 是否强制使用 SIGKILL（否则使用 SIGTERM）
    async fn kill_process_group(&self, force: bool) -> Result<()> {
        let pgid = self.inner.pgid;

        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            // 🔥 关键防御性检查：pgid 不能为 0
            // kill(0, SIGKILL) 会杀死调用者自己的进程组，这是危险的
            if pgid == 0 {
                warn!(
                    "[LifecycleGuard] 进程组 ID 为 0，跳过进程组终止（可能是初始化失败）: project_id={}",
                    self.inner.project_id
                );
                return Ok(());
            }

            // 🔥 关键：使用负的进程组 ID（真实的进程组 ID）
            // -pgid 表示发送信号到整个进程组，而不仅仅是进程组组长
            let target = Pid::from_raw(-(pgid as i32));

            let signal = if force {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };

            match kill(target, signal) {
                Ok(_) => {
                    debug!("already sent signal: pgid={}, signal={:?}", pgid, signal);

                    // 如果是 SIGTERM，等待一段时间让进程优雅退出
                    if !force {
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                        // 强制杀死进程组
                        let _ = kill(target, Signal::SIGKILL);
                        debug!("already force killed: pgid={}", pgid);
                    }
                }
                Err(Errno::ESRCH) => {
                    // 进程组已退出，这是正常的
                    debug!("process group already exited: pgid={}", pgid);
                }
                Err(Errno::EPERM) => {
                    // 权限不足，无法终止进程组
                    warn!(
                        "[LifecycleGuard] 权限不足，无法终止进程组: pgid={}, project_id={}",
                        pgid, self.inner.project_id
                    );
                }
                Err(e) => {
                    // 其他错误（如 EINVAL、EFAULT 等）
                    debug!(" kill failed: pgid={}, error={:?}", pgid, e);
                }
            }

            info!("Claude process group stopped: pgid={}", pgid);
        }

        #[cfg(not(unix))]
        {
            debug!("Unix platform, skipping process group stop");
        }

        Ok(())
    }
}

impl Clone for AgentLifecycleGuard {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AgentLifecycleGuard {
    fn drop(&mut self) {
        let strong_count = Arc::strong_count(&self.inner);

        debug!(
            "[Claude] AgentLifecycleGuard::drop 开始: project_id={}, pgid={}, strong_count={}",
            self.inner.project_id, self.inner.pgid, strong_count
        );

        // 🔥 使用原子 CAS 操作确保只执行一次清理
        // 不再依赖引用计数，因为引用计数可能因为多处 clone 而不准确
        // compare_exchange 返回 Ok 表示成功将 false 改为 true，即当前线程获得清理权
        // 返回 Err 表示已经被其他线程清理，当前线程无需操作
        let should_cleanup = self
            .inner
            .stopped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();

        if should_cleanup {
            debug!(
                "[Claude] AgentLifecycleGuard 获得清理权，开始清理资源: {}",
                self.inner.project_id
            );

            // 发送取消信号
            self.inner.cancel_token.cancel();

            // 注意：API 密钥配置的清理由 agent_runner 层的 stop_agent 方法统一负责
            // 包括：
            // - shared_api_key_manager 中的配置
            // - project_uuid_map 中的映射
            //
            // 这样避免双重清理，确保资源只被清理一次

            // 🔥 同步终止进程组
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;

                let pgid = self.inner.pgid;

                // 🔥 关键防御性检查：pgid 不能为 0
                // kill(0, SIGKILL) 会杀死调用者自己的进程组，这是危险的
                if pgid == 0 {
                    debug!(
                        "[Claude] 进程组 ID 为 0，跳过进程组终止: project_id={}",
                        self.inner.project_id
                    );
                } else {
                    let target = Pid::from_raw(-(pgid as i32));

                    if let Err(e) = kill(target, Signal::SIGKILL) {
                        // 进程可能已经退出，这是正常的
                        debug!(
                            "[Claude] 终止进程组失败（可能已退出）: pgid={}, error={}",
                            pgid, e
                        );
                    } else {
                        info!(
                            "[Claude] 进程组已终止: pgid={}, project_id={}",
                            pgid, self.inner.project_id
                        );
                    }
                }
            }

            #[cfg(not(unix))]
            {
                debug!("[Claude] Unix platform, skipping process group stop");
            }

            // 注意：后台回收任务 (reaper_task) 会自动完成
            // 不需要在这里等待或取消

            info!(
                "[Claude] AgentLifecycleGuard 清理完成: project_id={}",
                self.inner.project_id
            );
        } else {
            debug!(
                "[Claude] AgentLifecycleGuard 跳过清理（已被其他引用清理）: project_id={}",
                self.inner.project_id
            );
        }

        debug!(
            "[Claude] AgentLifecycleGuard::drop 完成: project_id={}",
            self.inner.project_id
        );
    }
}

// 为AgentLifecycleGuard实现AgentLifecycle trait
impl AgentLifecycle for AgentLifecycleGuard {
    fn graceful_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move { AgentLifecycleGuard::graceful_stop(self).await })
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

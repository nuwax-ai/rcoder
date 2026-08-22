//! agent 进程优雅停机（从 lifecycle.rs 拆出）。
//!
//! graceful_stop / cancel / 生命周期 Drop + Clone 语义。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::AgentLifecycleGuard;

/// 优雅停止agent
///
/// 带升级机制：SIGTERM → 等 500ms → SIGKILL（见 kill_process_group）。
/// 注：容器内子进程若为 PID 1 则信号被内核忽略，此时跳过进程组信号、
/// 仅发 cancel 信号并依赖 init 收割（见 kill_process_group 的 pgid==1 防御）。
///
/// ## 进程组终止
///
/// 使用 `process-wrap` 创建真正的进程组，发送信号到 `-pgid` 会终止：
/// - 子进程（进程组组长）
/// - 所有孙进程（同一进程组中的进程）
impl AgentLifecycleGuard {
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
    #[allow(unused_variables)]
    async fn kill_process_group(&self, force: bool) -> Result<()> {
        #[cfg(unix)]
        {
            let pgid = self.inner.pgid;
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

            // 🔥 PID 1 防御：子进程以 ProcessGroup::leader() 启动，pgid == child_pid。
            // 在 docker exec 等场景子进程可能拿到 pid 1，此时：
            // - kill(-1, SIG) 语义是「所有进程组」，绝不能发；
            // - 内核默认忽略 PID 1 的未注册信号（含 SIGTERM/SIGKILL）。
            // 依赖容器内 init（如 tini）兼作孤儿进程收割者（见本模块顶部说明）。
            if pgid == 1 {
                warn!(
                    "[LifecycleGuard] pgid==1（子进程为容器 PID 1），跳过进程组信号终止，\
                     仅依赖 cancel 信号与 init 收割: project_id={}",
                    self.inner.project_id
                );
                return Ok(());
            }

            // 🔥 关键：pgid 必须在 i32 范围内才能安全转换为负数
            // Linux PIDs 最大可达 4,194,304，远小于 i32::MAX (2,147,483,647)
            // 但为了防御性编程，仍然检查
            if pgid > i32::MAX as u32 {
                warn!(
                    "[LifecycleGuard] 进程组 ID {} 超出 i32 范围，跳过进程组终止: project_id={}",
                    pgid, self.inner.project_id
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
            debug!("Non-Unix platform, skipping process group stop");
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

                // 🔥 关键防御性检查：pgid 不能为 0 且必须在 i32 范围内
                // kill(0, SIGKILL) 会杀死调用者自己的进程组，这是危险的
                if pgid == 0 {
                    debug!(
                        "[Claude] 进程组 ID 为 0，跳过进程组终止: project_id={}",
                        self.inner.project_id
                    );
                } else if pgid > i32::MAX as u32 {
                    debug!(
                        "[Claude] 进程组 ID {} 超出 i32 范围，跳过进程组终止: project_id={}",
                        pgid, self.inner.project_id
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
                debug!("[Claude] Non-Unix platform, skipping process group stop");
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

//! agent 进程拉起（从 lifecycle.rs 拆出）。
//!
//! ClaudeProcessParams + new_claude/new_claude_full（参数/env/cwd 组装 → spawn）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::SessionId;
use anyhow::Result;
use dashmap::DashMap;
use process_wrap::tokio::ChildWrapper;
use shared_types::ModelProviderConfig;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::diagnostics::{DiagnosticsListener, ProcessDiagnostics};

use super::{
    AgentLifecycleGuard, AgentLifecycleInner, AgentResources, ExitDetail, analyze_exit_detail,
};

/// Claude 进程启动参数
///
/// 将多个构造函数参数封装为结构体，提升可读性和可维护性。
pub struct ClaudeProcessParams {
    pub project_id: String,
    pub session_id: SessionId,
    pub child_process: Box<dyn ChildWrapper>,
    pub stderr_task: JoinHandle<()>,
    pub cancel_token: CancellationToken,
    pub shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    pub project_uuid_map: Option<Arc<DashMap<String, String>>>,
    pub service_uuid: Option<String>,
    pub abnormal_exit_flag: Option<Arc<AtomicBool>>,
    /// 🔥 新增：详细的退出信息（signal、exit_code），用于生成更有意义的错误消息
    pub exit_detail: Option<Arc<Mutex<Option<ExitDetail>>>>,
    pub diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
    pub process_command: String,
    pub process_args: Vec<String>,
    pub working_dir: PathBuf,
}

impl AgentLifecycleGuard {
    /// 为Claude Agent创建生命周期守卫（便捷入口，默认无密钥管理器/诊断监听器）
    ///
    /// **注意**：此构造函数不携带 `diagnostics_listener`、`process_command` 等诊断信息，
    /// reaper 退出时不会触发 `on_process_error` / `on_process_exited` 回调。
    /// 如需完整诊断能力，请使用 `new_claude_full`。
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
    ) -> Result<Self> {
        Self::new_claude_full(ClaudeProcessParams {
            project_id,
            session_id,
            child_process,
            stderr_task,
            cancel_token,
            shared_api_key_manager: None,
            project_uuid_map: None,
            service_uuid: None,
            abnormal_exit_flag: None,
            exit_detail: None,
            diagnostics_listener: None,
            process_command: String::new(),
            process_args: Vec::new(),
            working_dir: PathBuf::new(),
        })
    }

    /// 完整构造函数：通过结构体参数创建生命周期守卫
    ///
    /// 支持所有可选功能：密钥管理器、异常退出标志、诊断监听器等。
    ///
    /// # Errors
    ///
    /// 子进程 PID 不存在或为 0 时返回初始化错误。
    pub fn new_claude_full(params: ClaudeProcessParams) -> Result<Self> {
        let ClaudeProcessParams {
            project_id,
            session_id,
            mut child_process,
            stderr_task,
            cancel_token,
            shared_api_key_manager,
            project_uuid_map,
            service_uuid,
            abnormal_exit_flag,
            exit_detail,
            diagnostics_listener,
            process_command,
            process_args,
            working_dir,
        } = params;
        let pid = child_process.id().filter(|pid| *pid != 0).ok_or_else(|| {
            anyhow::anyhow!("lifecycle child process has no valid PID: project_id={project_id}")
        })?;

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
        let exit_detail_clone = exit_detail.clone();
        let project_id_for_reaper = project_id.clone();
        let listener_for_reaper = diagnostics_listener.clone();
        let command_for_reaper = process_command.clone();
        let args_for_reaper = process_args.clone();
        let working_dir_for_reaper = working_dir.clone();
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

            let mut should_report_error = false;
            let mut exit_code_opt: Option<i32> = None;
            let mut error_msg: Option<String> = None;

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

                    exit_code_opt = exit_code;

                    if !status.success() {
                        // 🔥 非零退出码或被信号杀死 = 异常退出
                        if let Some(ref flag) = abnormal_exit_flag_clone {
                            // 只有非用户主动取消时才标记为异常
                            if !was_cancelled {
                                flag.store(true, Ordering::SeqCst);
                            }
                        }
                        // 🔥 新增：设置详细的退出信息，用于生成更有意义的错误消息
                        if let Some(ref exit_detail) = exit_detail_clone
                            && !was_cancelled
                        {
                            let detail = analyze_exit_detail(exit_code, signal);
                            let mut guard = exit_detail.lock().await;
                            *guard = Some(detail);
                        }
                        // 只有非用户主动取消时才视为需要报告错误
                        if !was_cancelled {
                            should_report_error = true;
                            error_msg = Some(format!(
                                "non-zero exit (code={:?}, signal={:?})",
                                exit_code, signal
                            ));
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
                    if !was_cancelled {
                        should_report_error = true;
                        error_msg = Some(format!("wait() failed: {}", e));
                    }
                    warn!(
                        "[ProcessReaper] Agent 进程 wait() 失败: project_id={}, pid={}, pgid={}, error={}, was_cancelled={}",
                        project_id_for_reaper, pid, pgid, e, was_cancelled
                    );
                }
            }

            // P0-2 接线: 把退出事件转给 listener
            // 规则: 非用户主动取消 + 异常(非 0 退出 / wait 失败) → on_process_error
            //       其他情况(0 退出 / 用户主动取消) → on_process_exited
            if let Some(ref listener) = listener_for_reaper {
                let diagnostics = ProcessDiagnostics {
                    command: command_for_reaper,
                    args: args_for_reaper,
                    working_dir: working_dir_for_reaper,
                    pid,
                    exit_code: exit_code_opt,
                    stderr_tail: Vec::new(),
                    command_exists: true,
                    startup_duration_ms: 0,
                    acp_init_success: false,
                    error_message: error_msg,
                };
                if should_report_error {
                    listener.on_process_error(&diagnostics);
                } else {
                    listener.on_process_exited(&diagnostics);
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
            diagnostics_listener,
            process_command,
            process_args,
            working_dir,
        });

        info!(
            "[LifecycleGuard] 创建 Claude Agent 守卫: project_id={}, pgid={}, session_id={}",
            project_id, pgid, session_id_str
        );

        Ok(Self { inner })
    }
}

//! Agent 子进程构建/启动与会话等待
//!
//! 从 `SacpClaudeCodeLauncher::launch` 抽出的进程构建/启动部分：
//! - 子进程构建与启动（平台 cfg 分支集中于此）
//! - stderr 读取任务
//! - session_id 等待（超时/连接失败快速通道/通道关闭三分支）

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionId;
use anyhow::{Context, Result};
use process_wrap::tokio::ChildWrapper;
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use shared_types::error_codes;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
#[cfg(windows)]
use windows::Win32::System::Threading::PROCESS_CREATION_FLAGS;

#[cfg(windows)]
use super::super::windows_launch::CREATE_NO_WINDOW_FLAG;
use super::process::take_stdio;

/// Agent 子进程启动结果
pub(super) struct SpawnedAgentProcess {
    pub(super) child: Box<dyn ChildWrapper>,
    pub(super) child_pid: u32,
    pub(super) stdin: tokio::process::ChildStdin,
    pub(super) stdout: tokio::process::ChildStdout,
    pub(super) stderr: tokio::process::ChildStderr,
}

/// 启动子进程（使用进程组/Job Object 来管理整个进程树）
/// Unix: ProcessGroup::leader() 创建进程组，确保能够清理所有孙进程
/// Windows: JobObject 管理进程树
pub(super) fn spawn_agent_process(
    command_path: &str,
    command_args: &[String],
    merged_envs: &HashMap<String, String>,
    project_path: &Path,
    full_command_line: &str,
) -> Result<SpawnedAgentProcess> {
    info!(
        "[SACP] Spawning subprocess: cmd=[{}], cwd={}",
        full_command_line,
        project_path.display()
    );
    let mut cmd_wrap = CommandWrap::with_new(command_path, |cmd| {
        cmd.args(command_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(project_path);
        cmd.envs(merged_envs);
    });

    #[cfg(unix)]
    let mut child = cmd_wrap
        .wrap(ProcessGroup::leader())
        .spawn()
        .context("[SACP] Failed to start ACP subprocess")?;

    #[cfg(windows)]
    let mut child = cmd_wrap
        .wrap(CreationFlags(PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW_FLAG)))
        .wrap(JobObject)
        .spawn()
        .context("[SACP] Failed to start ACP subprocess")?;

    #[cfg(not(any(unix, windows)))]
    compile_error!("neither unix nor windows");

    let child_pid = child.id().unwrap_or(0);
    info!(
        "[SACP] Claude Code ACP child process already started, PID: {}",
        child_pid
    );

    // 获取 stdio 句柄（process_wrap 使用方法访问 stdio）
    let stdin = take_stdio(child.stdin(), "stdin")?;
    let stdout = take_stdio(child.stdout(), "stdout")?;
    let stderr = take_stdio(child.stderr(), "stderr")?;

    Ok(SpawnedAgentProcess {
        child,
        child_pid,
        stdin,
        stdout,
        stderr,
    })
}

/// 🔥 立即启动 stderr 读取任务（在 session_id 等待之前）
/// 这样即使子进程在初始化阶段就退出，也能捕获 stderr 输出
pub(super) fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    cancel_token: &CancellationToken,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let cancel_token_for_stderr = cancel_token.clone();
    let stderr_output_shared = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stderr_output_clone = stderr_output_shared.clone();
    let stderr_task_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();

        loop {
            tokio::select! {
                biased; // 优先检查取消信号

                _ = cancel_token_for_stderr.cancelled() => {
                    debug!("[SACP] stderr cancel received");
                    break;
                }
                result = lines.next_line() => {
                    match result {
                        Ok(Some(line)) if !line.trim().is_empty() => {
                            warn!("[SACP] ACP Agent stderr: {}", line.trim());
                            // 存储 stderr 输出，用于错误传播
                            if let Ok(mut buf) = stderr_output_clone.lock() {
                                buf.push(line.trim().to_string());
                                // 限制最多存储 20 行，避免内存膨胀
                                if buf.len() > 20 {
                                    buf.remove(0);
                                }
                            }
                        }
                        Ok(Some(_)) => {} // 空行，忽略
                        Ok(None) => break, // EOF
                        Err(e) => {
                            error!("[SACP] read stderr failed: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    });
    (stderr_task_handle, stderr_output_shared)
}

/// 等待会话 ID（超时取自 start_config / GrpcTimeoutConfig，默认 60s），同时监听连接失败
#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_for_session_id(
    project_id: &str,
    session_create_timeout_secs: u64,
    session_id_rx: tokio::sync::oneshot::Receiver<SessionId>,
    connection_failed_rx: tokio::sync::oneshot::Receiver<String>,
    connection_task_handle: &tokio::task::JoinHandle<()>,
    stderr_output_shared: &Arc<std::sync::Mutex<Vec<String>>>,
    connection_error_shared: &Arc<std::sync::Mutex<Option<String>>>,
    full_command_line: &str,
    child_pid: u32,
) -> Result<SessionId> {
    info!(
        "[SACP] Waiting for session_id from ACP agent, project_id={}, timeout={}s",
        project_id, session_create_timeout_secs
    );
    let session_id = match tokio::time::timeout(
        std::time::Duration::from_secs(session_create_timeout_secs),
        async {
            tokio::select! {
                result = session_id_rx => {
                    match result {
                        Ok(sid) => Ok(Ok(sid)),
                        Err(e) => Ok(Err(anyhow::anyhow!("channel dropped: {}", e))),
                    }
                }
                failed = connection_failed_rx => {
                    match failed {
                        Ok(err_msg) => Err(anyhow::anyhow!("{}", err_msg)),
                        Err(_) => Ok(Err(anyhow::anyhow!("connection ended without session_id or error"))),
                    }
                }
            }
        },
    )
    .await
    {
        Ok(Ok(Ok(session_id))) => {
            info!(
                "[SACP] Received session_id from ACP agent: {}, project_id={}",
                session_id, project_id
            );
            session_id
        }
        Err(_timeout_elapsed) => {
            // 60 秒超时，连接任务仍在运行
            let stderr_info = stderr_output_shared.lock().ok()
                .map(|buf| buf.join("\n"))
                .filter(|s| !s.is_empty())
                .map(|s| format!("; stderr: {}", s))
                .unwrap_or_default();
            error!(
                "[SACP] Agent initialization timeout ({}s), project_id={}, command=[{}], child_pid={}, stderr={}",
                session_create_timeout_secs, project_id, full_command_line, child_pid, stderr_info
            );
            // 超时后取消 spawned 任务，避免子进程泄漏
            connection_task_handle.abort();
            // kill 子进程（使用进程组 kill 清理所有孙进程）
            #[cfg(unix)]
            {
                use nix::errno::Errno;
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                if child_pid > 1 {
                    // kill(2) 的负数 pid 表示「整个进程组」；子进程以 ProcessGroup::leader() 启动，pgid == child_pid，
                    // 故 -child_pid 能杀掉 claude-code-acp-ts 及其所有 MCP 孙进程。
                    // ⚠️ 绝不能用 libc killpg 并预先取负：killpg 期望正数 pgrp（内部自取负），负数会直接 EINVAL。
                    // 与 lifecycle.rs 的正常关闭路径保持一致。
                    // ⚠️ child_pid==1 时 kill(-1) 语义是「所有进程组」且 PID 1 信号被内核忽略，必须跳过。
                    let target = Pid::from_raw(-(child_pid as i32));
                    match kill(target, Signal::SIGKILL) {
                        Ok(_) => warn!(
                            "[SACP] Killed process group (SIGKILL) for child_pid={}, project_id={}",
                            child_pid, project_id
                        ),
                        Err(Errno::ESRCH) => debug!(
                            "[SACP] Process group already exited: child_pid={}, project_id={}",
                            child_pid, project_id
                        ),
                        Err(e) => error!(
                            "[SACP] Failed to kill process group for child_pid={}: {}, project_id={}",
                            child_pid, e, project_id
                        ),
                    }
                } else if child_pid == 1 {
                    warn!(
                        "[SACP] child_pid==1（容器 PID 1），跳过进程组 kill，依赖 init 收割: project_id={}",
                        project_id
                    );
                }
            }
            return Err(anyhow::anyhow!(
                "{}: agent initialization timeout ({}s){}",
                error_codes::get_i18n_message_default("error.agent_init_timeout"),
                session_create_timeout_secs,
                stderr_info
            ));
        }
        Ok(Err(e)) => {
            // 连接任务主动报告了失败，立即返回
            let err_str = e.to_string();
            let stderr_info = stderr_output_shared.lock().ok()
                .map(|buf| buf.join("\n"))
                .filter(|s| !s.is_empty())
                .map(|s| format!("; stderr: {}", s))
                .unwrap_or_default();
            let clean_msg = err_str
                .strip_prefix("connection failed: ")
                .unwrap_or(&err_str);
            error!(
                "[SACP] Agent connection failed early: project_id={}, error={}, stderr={}",
                project_id, err_str, stderr_info
            );
            return Err(anyhow::anyhow!(
                "Agent process failed: {}{}",
                clean_msg,
                stderr_info
            ));
        }
        Ok(Ok(Err(e))) => {
            // channel dropped — 读取连接任务的实际错误原因
            let connection_error = connection_error_shared.lock().ok()
                .and_then(|guard| guard.clone())
                .unwrap_or_else(|| "unknown error".to_string());
            // 读取 stderr 输出
            let stderr_info = stderr_output_shared.lock().ok()
                .map(|buf| buf.join("\n"))
                .filter(|s| !s.is_empty())
                .map(|s| format!("; stderr: {}", s))
                .unwrap_or_default();
            error!(
                "[SACP] session_id channel dropped (connection task failed): recv_error={}, actual_error={}, project_id={}",
                e, connection_error, project_id
            );
            // 连接任务已自行结束，无需 abort
            return Err(anyhow::anyhow!(
                "{}: {}{}",
                error_codes::get_i18n_message_default("error.agent_init_timeout"),
                connection_error,
                stderr_info
            ));
        }
    };

    info!(
        "[SACP] Claude Code ACP Agent service started successfully, session ID: {}",
        session_id
    );

    Ok(session_id)
}

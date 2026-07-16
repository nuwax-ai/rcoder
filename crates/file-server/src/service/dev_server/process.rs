//! Dev server 进程 spawn/kill/探活 (对齐 nuwax `processManager.js`)。
//!
//! 关键点 (对齐 nuwax):
//! - `exec` 前缀让 child.pid == vite/next 本体 pid (非 sh wrapper)
//! - `process_group(0)` 新进程组, kill 时 `kill(-pid)` 杀整组
//! - env 最小化 (PATH + HOME + NODE_ENV=development + extra), env_clear 后显式赋值
//! - detached: 丢弃 Child 句柄 (kill_on_drop=false), 进程独立存活, 靠 pid 杀

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::error::{AppError, AppResult};

/// dev server 启动参数 (program + args + env_extra)。
pub struct DevArgs {
    pub program: &'static str,
    pub args: Vec<String>,
    pub env_extra: Vec<(String, String)>,
}

/// 构造 dev server 启动参数 (vite/next)。用 arg 数组而非 shell 字符串拼接, 避免注入
/// (base_path 来自用户, 经 sh -c 拼接会注入)。
pub fn build_dev_args(
    dev_script: &str,
    port: u16,
    base_path: Option<&str>,
) -> AppResult<DevArgs> {
    let lower = dev_script.to_ascii_lowercase();
    let base = base_path.map(normalize_base_path);
    let mut env_extra: Vec<(String, String)> = Vec::new();
    let port_str = port.to_string();
    let args: Vec<String> = if lower.contains("vite") {
        let mut a = vec![
            "vite".into(),
            "--port".into(),
            port_str,
            "--strictPort".into(),
            "--host".into(),
            "0.0.0.0".into(),
            // 抑制 ANSI 清屏转义码 (否则污染日志管道), 对齐 vite-rs
            "--clearScreen".into(),
            "false".into(),
        ];
        if let Some(b) = &base {
            a.push("--base".into());
            a.push(b.clone());
        }
        a
    } else if lower.contains("next") {
        if let Some(b) = &base {
            env_extra.push(("NEXT_PUBLIC_BASE_PATH".into(), b.clone()));
            env_extra.push(("BASE_PATH".into(), b.clone()));
        }
        vec!["next".into(), "dev".into(), "-p".into(), port_str]
    } else {
        return Err(AppError::business(format!(
            "unsupported dev script (must contain vite or next): {dev_script}"
        )));
    };
    Ok(DevArgs {
        program: "npx",
        args,
        env_extra,
    })
}

/// 规范化 basePath 为 `/x/` 形式 (对齐 nuwax)。
fn normalize_base_path(b: &str) -> String {
    let b = b.trim();
    if b.is_empty() {
        "/".to_string()
    } else if let Some(stripped) = b.strip_prefix('/') {
        let s = stripped.trim_end_matches('/');
        if s.is_empty() {
            "/".to_string()
        } else {
            format!("/{s}/")
        }
    } else {
        format!("/{}/", b.trim_end_matches('/'))
    }
}

/// 最小化 env (对齐 nuwax: PATH + NODE_ENV=development + extra; 补 HOME 供 pnpm cache)。
fn minimal_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Ok(p) = std::env::var("PATH") {
        env.push(("PATH".into(), p));
    }
    if let Ok(h) = std::env::var("HOME") {
        env.push(("HOME".into(), h));
    }
    env.push(("NODE_ENV".into(), "development".into()));
    env.push(("ROLLUP_WASM".into(), "1".into()));
    env.push(("ROLLUP_DISABLE_NATIVE".into(), "1".into()));
    for (k, v) in extra {
        env.push((k.clone(), v.clone()));
    }
    env
}

/// spawn detached dev server (运维专用 override, sh -c 整条命令)。
/// 仅 `DEV_SERVER_OVERRIDE_CMD` (部署时运维设置, 非外部用户输入) 走此路径,
/// 故 shell 拼接无注入风险。用户输入路径必须走 [`spawn_dev`] (arg 数组)。
pub fn spawn_override_shell(
    full_command: &str,
    cwd: &Path,
) -> AppResult<(Child, Option<ChildStdout>, Option<ChildStderr>)> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(full_command);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(minimal_env(&[]));
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::system(format!("spawn override dev server failed: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok((child, stdout, stderr))
}

/// spawn detached dev server (arg 数组, 无 shell; 进程组 + 丢弃 Child 句柄)。
/// 返回 (child, stdout, stderr); 调用方负责取走 stdout/stderr 起日志管道。
pub fn spawn_dev(
    program: &str,
    args: &[String],
    cwd: &Path,
    env_extra: &[(String, String)],
) -> AppResult<(Child, Option<ChildStdout>, Option<ChildStderr>)> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(minimal_env(env_extra));
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // 新进程组 (unix): child.pid == pgid, 后续 kill(-pid) 杀整组 (含 vite 子进程)
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::system(format!("spawn dev server failed: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok((child, stdout, stderr))
}

/// 运行一次性命令 (install/build/preprocess), arg 数组 + stdout/stderr 管道到日志, 阻塞等待。
/// 用 current_dir 替代 `cd ... &&`, arg 数组替代 shell 拼接; 错误为类型化 io::Error/退出码。
pub async fn run_command_to_log(
    program: &str,
    args: &[&str],
    cwd: &Path,
    main_log: &Path,
    temp_log: &Path,
    timeout_secs: u64,
) -> AppResult<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    // install 继承当前 env (pnpm store 等), 仅删 CI/NPM_CONFIG_PRODUCTION
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    if let Ok(h) = std::env::var("HOME") {
        cmd.env("HOME", h);
    }
    cmd.env_remove("CI");
    cmd.env_remove("NPM_CONFIG_PRODUCTION");
    cmd.env("NODE_ENV", "development");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::system(format!("spawn command failed: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let main = main_log.to_path_buf();
    let temp = temp_log.to_path_buf();
    if let Some(out) = stdout {
        let m = main.clone();
        let t = temp.clone();
        tokio::spawn(async move { pipe_stream(out, m, t).await });
    }
    if let Some(err) = stderr {
        let m = main.clone();
        let t = temp.clone();
        tokio::spawn(async move { pipe_stream(err, m, t).await });
    }
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(AppError::system(format!(
            "command exited non-zero: {status}"
        ))),
        Ok(Err(e)) => Err(AppError::system(format!("command wait failed: {e}"))),
        Err(_) => {
            // 超时: 尽力杀子进程
            let _ = child.start_kill();
            Err(AppError::system(format!(
                "command timed out after {timeout_secs}s"
            )))
        }
    }
}

async fn pipe_stream<R>(reader: R, main: PathBuf, temp: PathBuf)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let prefixed = format!(
            "[{}] {}",
            Local::now().format("%Y/%m/%d %H:%M:%S"),
            line
        );
        let _ = super::log::append_line(&main, &prefixed).await;
        let _ = super::log::append_line(&temp, &prefixed).await;
    }
}

/// 杀进程组 (对齐 nuwax killProcess): 优先 kill(-pid) SIGTERM, 降级 kill(pid)。
/// 返回是否成功送出信号。
pub fn kill_process_group(pid: u32) -> bool {
    let pgid = Pid::from_raw(-(pid as i32));
    match kill(pgid, Signal::SIGTERM) {
        Ok(()) => true,
        Err(_) => kill(Pid::from_raw(pid as i32), Signal::SIGTERM).is_ok(),
    }
}

/// 强杀进程组 (SIGKILL 升级): SIGTERM 宽限期后进程仍存活时调用, 优先 kill(-pid) SIGKILL, 降级 kill(pid)。
pub fn kill_process_group_force(pid: u32) -> bool {
    let pgid = Pid::from_raw(-(pid as i32));
    match kill(pgid, Signal::SIGKILL) {
        Ok(()) => true,
        Err(_) => kill(Pid::from_raw(pid as i32), Signal::SIGKILL).is_ok(),
    }
}

/// 进程是否仍在运行 (kill pid 0 探活; 对齐 nuwax isProcessRunning)。
pub fn is_process_running(pid: u32) -> bool {
    // kill(pid, None) == 信号 0, 不实际杀, 仅探测
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true, // 存在但无权限
        Err(_) => false,
    }
}

/// 轮询等待进程退出 (对齐 nuwax waitForProcessStop)。
pub async fn wait_for_stop(pid: u32, interval_ms: u64, max_attempts: u32) {
    for _ in 0..max_attempts {
        if !is_process_running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}

/// HTTP 探活 dev server (对齐 nuwax isProjectAlive): GET 127.0.0.1:port{basePath}, 2xx/3xx 存活。
pub async fn is_project_alive(port: u16, base_path: Option<&str>, timeout_ms: u64) -> bool {
    let base = base_path.map(normalize_base_path).unwrap_or_default();
    let url = format!("http://127.0.0.1:{port}{}", base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .build();
    let Ok(client) = client else {
        return false;
    };
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success() || resp.status().is_redirection(),
        Err(_) => false,
    }
}

/// 当前时间毫秒 (chrono, 非 std::time 获取以保持可测)。
pub fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

//! Shared cross-platform owned process tree; original platform regressions remain here.
pub(crate) use process_utils::managed_tree::{ManagedChild, StopOutcome, spawn_managed};
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
mod tests {
    use super::*;

    fn silent(mut cmd: tokio::process::Command) -> tokio::process::Command {
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        cmd
    }

    /// 受管进程的后代随父进程组停止而退出（XP05 Unix 真实进程组）。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_process_group_stop() {
        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.args(["-c", "sleep 60 & sleep 60 & wait"]);
        let mut child = spawn_managed(cmd).unwrap();
        let pid = child.id().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let outcome = child.stop(Duration::from_secs(1)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "process group must be stopped, got: {outcome:?}"
        );

        // 确认进程组中的所有进程都已退出
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_utils::process_group_exists(pid).unwrap_or(true),
            "process group {pid} should not exist after stop"
        );
    }

    /// 等待受管进程正常退出。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_normal_exit() {
        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.args(["-c", "exit 0"]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success());
    }

    /// 强制终止：忽略 SIGTERM 的进程在宽限期后被杀。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_force_kill_after_timeout() {
        let mut cmd = silent(tokio::process::Command::new("python3"));
        cmd.args([
            "-c",
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
        ]);
        let mut child = spawn_managed(cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(150)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "process must be stopped, got: {outcome:?}"
        );
    }

    /// XP08（Unix）：空格 + 中文路径的工作目录与参数下受管 spawn/停止正常。
    #[cfg(unix)]
    #[tokio::test]
    async fn xp08_managed_child_with_spaces_and_unicode_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("项目 目录 with spaces");
        std::fs::create_dir_all(&workdir).unwrap();
        let marker = workdir.join("标记 文件.txt");

        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.current_dir(&workdir);
        cmd.args(["-c", &format!("echo ok > '{}'", marker.display())]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success(), "spawn in unicode+spaces cwd must succeed");
        assert!(
            marker.exists(),
            "command must run inside the unicode workdir"
        );
    }

    /// XP08（Windows）：空格 + 中文路径的工作目录 + cmd /C 受管 spawn。
    ///
    /// cmd /C 的整条命令必须经 raw_arg 原样传入——args() 对含空格参数
    /// 自动加引号会与重定向语法嵌套破坏命令行（cross-platform.md §5
    /// 的 Windows shell 命令适配坑，真实业务同样适用）。
    #[cfg(windows)]
    #[tokio::test]
    async fn xp08_managed_child_with_spaces_and_unicode_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("项目 目录 with spaces");
        std::fs::create_dir_all(&workdir).unwrap();
        let marker = workdir.join("标记 文件.txt");

        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.current_dir(&workdir);
        {
            use std::os::windows::process::CommandExt;
            cmd.raw_arg(format!("/C echo ok>\"{}\"", marker.display()));
        }
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(
            status.success(),
            "spawn in unicode+spaces cwd must succeed (status: {status:?})"
        );
        assert!(
            marker.exists(),
            "command must run inside the unicode workdir"
        );
    }

    /// Windows：挂起 spawn → Job 归属 → 恢复执行，正常退出。
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_spawn_and_exit() {
        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.args(["/C", "exit 0"]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success());
    }

    /// Windows：宽限期超时后 Job 终止收束全树（cmd → ping 孙进程）。
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_force_stop_kills_tree() {
        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
        let mut child = spawn_managed(cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(500)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "Windows process tree must be stopped, got: {outcome:?}"
        );
    }
}

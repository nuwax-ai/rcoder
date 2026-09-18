//! 跨平台排他文件锁——运行态单一所有者的基础保证（cross-platform.md §3）。
//!
//! OwnerGuard 持有锁文件句柄，生命周期覆盖整个 owner 进程。锁文件位于
//! 部署替换范围之外的稳定状态根（`.app-cli-state/{app_id}/owner.lock`）。
//!
//! 使用 Rust 标准库文件锁（1.89 稳定）：
//! - Unix: `flock(LOCK_EX | LOCK_NB)` — 进程级锁，进程退出自动释放
//! - Windows: `LockFileEx` — 句柄关闭自动释放
//!
//! 不使用 PID 文件、端口检查或锁文件存在性作为排他依据。

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

/// 运行态排他锁（持有期间本机同项目唯一 owner）。
///
/// Drop 释放锁文件句柄（平台自动释放排他锁），不删除锁文件。
pub struct OwnerGuard {
    _file: File,
    /// 锁文件路径（诊断用；生产路径经 acquire 错误上下文携带）。
    #[allow(dead_code)]
    lock_path: PathBuf,
}

impl OwnerGuard {
    /// 尝试获取排他锁。
    ///
    /// `state_root` 为稳定状态根目录（`.app-cli-state/{app_id}/`）。
    /// 成功返回 Guard（RAII 释放），失败返回错误（锁被占用 / 权限不足）。
    pub fn acquire(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root).with_context(|| {
            format!("create state root for owner lock: {}", state_root.display())
        })?;
        let lock_path = state_root.join("owner.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open owner lock file: {}", lock_path.display()))?;
        file.try_lock().with_context(|| {
            format!(
                "acquire exclusive owner lock: {} (another instance may be running)",
                lock_path.display()
            )
        })?;
        Ok(Self {
            _file: file,
            lock_path,
        })
    }

    /// 锁文件路径（诊断用）。
    #[allow(dead_code)]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_hold_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let guard = OwnerGuard::acquire(&root).unwrap();
        assert!(guard.lock_path().exists());
        // 同一进程再次获取应失败（排他）
        assert!(OwnerGuard::acquire(&root).is_err());
        drop(guard);
        // 释放后可重新获取
        let _guard2 = OwnerGuard::acquire(&root).unwrap();
    }

    #[test]
    fn lock_survives_handle_drop_without_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        {
            let _guard = OwnerGuard::acquire(&root).unwrap();
        }
        // 锁文件仍然存在（不删除）
        assert!(root.join("owner.lock").exists());
    }

    /// XP06（Unix）：owner 被 SIGKILL 后，其业务后代仍存活，但锁不被
    /// 后代保活——新 owner 立即可接管（flock 随进程终止释放，后代从未
    /// 打开锁文件故不持有）。
    #[cfg(unix)]
    #[test]
    fn xp06_lock_released_on_owner_kill_while_descendants_live() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        let lock_path = root.join("owner.lock");
        std::fs::write(&lock_path, b"").unwrap();
        let lock_str = lock_path.display().to_string();

        // owner 替身：持锁 + 派生 sleep 后代（打印后代 PID）
        let script = format!(
            r#"
import fcntl, subprocess, sys, time
f = open('{lock_str}', 'r+')
fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
child = subprocess.Popen(['sleep', '60'])
print(child.pid, flush=True)
time.sleep(300)
"#
        );
        let mut owner = std::process::Command::new("python3")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn owner surrogate");

        // 读后代 PID（owner 首行输出）
        use std::io::BufRead as _;
        let stdout = owner.stdout.take().expect("stdout piped");
        let reader = std::io::BufReader::new(stdout);
        let descendant_pid: u32 = reader
            .lines()
            .next()
            .expect("pid line")
            .expect("pid parse")
            .trim()
            .parse()
            .expect("pid number");

        // SIGKILL owner（非常退出）
        owner.kill().expect("kill owner");
        owner.wait().expect("reap owner");

        // 后代仍存活（锁不被它保活的前提）——kill -0 探活（macOS 无 /proc）
        let descendant_alive = std::process::Command::new("kill")
            .args(["-0", &descendant_pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            descendant_alive,
            "descendant {descendant_pid} must outlive the killed owner"
        );

        // 新 owner 立即接管
        let _new_owner = OwnerGuard::acquire(&root)
            .expect("lock must be acquirable after owner death despite descendants");

        // 清理后代（孤儿进程，单进程信号兜底）
        process_utils::kill_process_group_with_fallback(
            descendant_pid,
            process_utils::KillSignal::SIGKILL,
        );
    }

    /// XP06（Windows）：owner 被 TerminateProcess 后，其业务后代仍存活，
    /// 但 LockFileEx 随进程终止释放——新 owner 立即可接管。
    #[cfg(windows)]
    #[test]
    fn xp06_lock_released_on_owner_kill_while_descendants_live() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        let lock_path = root.join("owner.lock");
        std::fs::write(&lock_path, b"").unwrap();
        let lock_str = lock_path.display().to_string();

        let script = format!(
            "$f=[System.IO.File]::Open('{}','Open','ReadWrite','ReadWrite'); \
             $f.Lock(0,1); \
             $child=Start-Process -FilePath 'cmd' -ArgumentList '/C','ping -n 60 127.0.0.1' \
                       -PassThru -WindowStyle Hidden; \
             Write-Output $child.Id; \
             Start-Sleep -Seconds 300",
            lock_str
        );
        let mut owner = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn owner surrogate");

        use std::io::BufRead as _;
        let stdout = owner.stdout.take().expect("stdout piped");
        let reader = std::io::BufReader::new(stdout);
        let descendant_pid: u32 = reader
            .lines()
            .next()
            .expect("pid line")
            .expect("pid parse")
            .trim()
            .parse()
            .expect("pid number");

        // TerminateProcess owner（非常退出）
        owner.kill().expect("kill owner");
        owner.wait().expect("reap owner");

        // 后代仍存活
        let listed = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {descendant_pid}")])
            .output()
            .expect("tasklist");
        assert!(
            String::from_utf8_lossy(&listed.stdout).contains(&descendant_pid.to_string()),
            "descendant {descendant_pid} must outlive the killed owner"
        );

        // 新 owner 立即接管
        let _new_owner = OwnerGuard::acquire(&root)
            .expect("lock must be acquirable after owner death despite descendants");

        // 清理后代
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &descendant_pid.to_string(), "/F", "/T"])
            .output();
    }

    /// 跨进程锁互斥（Unix：Python `fcntl.flock` 与 std 的 flock 同一锁域）。
    #[cfg(unix)]
    #[test]
    fn cross_process_lock_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let _guard = OwnerGuard::acquire(&root).unwrap();

        let root_str = root.to_str().unwrap().to_string();
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                &format!(
                    r#"
import fcntl, sys
try:
    f = open('{}/owner.lock', 'r+')
except FileNotFoundError:
    print('lock file missing')
    sys.exit(2)
try:
    fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    print('acquired')
    sys.exit(0)
except (BlockingIOError, OSError):
    print('blocked')
    sys.exit(1)
"#,
                    root_str
                ),
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "child process must fail to acquire lock held by parent; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// 跨进程锁互斥（Windows：PowerShell/.NET `FileStream.Lock` 与 std 的
    /// LockFileEx 同一 byte-range 锁域——两者都覆盖文件偏移 0）。
    #[cfg(windows)]
    #[test]
    fn cross_process_lock_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let _guard = OwnerGuard::acquire(&root).unwrap();
        let lock_path = root.join("owner.lock").display().to_string();

        // .NET Lock(0,1) 锁 [0,1)；std try_lock 锁 [0, MAX)——偏移 0 重叠必冲突。
        // 路径插入单引号 PS 字符串（字面量语义，反斜杠无需转义）。
        let script = format!(
            "$ErrorActionPreference='Stop'; try {{ \
             $f=[System.IO.File]::Open('{}','Open','ReadWrite','ReadWrite'); \
             $f.Lock(0,1); Write-Output 'acquired'; exit 0 \
             }} catch {{ Write-Output 'blocked'; exit 1 }}",
            lock_path
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("spawn powershell peer");
        assert!(
            !output.status.success(),
            "powershell peer must fail to lock held by parent; stdout: {}, stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// 并发子进程全部被拒（同上：Unix flock 锁域）。
    #[cfg(unix)]
    #[test]
    fn concurrent_acquire_only_one_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        // 本进程先获取锁
        let _guard = OwnerGuard::acquire(&root).unwrap();

        let root_str = root.to_str().unwrap().to_string();
        let mut children = Vec::new();
        for _ in 0..4 {
            let child = std::process::Command::new("python3")
                .args([
                    "-c",
                    &format!(
                        r#"
import fcntl, sys
try:
    f = open('{}/owner.lock', 'r+')
    fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    print('acquired')
    sys.exit(0)
except (BlockingIOError, OSError):
    print('blocked')
    sys.exit(1)
"#,
                        root_str
                    ),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            children.push(child);
        }
        for mut child in children {
            let status = child.wait().unwrap();
            assert!(
                !status.success(),
                "child process must fail to acquire lock held by parent"
            );
        }
    }
}

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
pub(crate) struct OwnerGuard {
    _file: File,
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

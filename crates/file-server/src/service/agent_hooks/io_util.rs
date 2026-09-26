//! 原子文件写入辅助 (对齐 nuwax `writeFileAtomic` / `writeJsonFileAtomic`)。
//!
//! 供 agent_hooks 各子模块复用: staging 写 codex 脚本 / settings.json / hook 脚本等。

use std::io::Write;
use std::path::Path;

use serde_json::Value;
use tokio::fs;

use crate::error::{AppError, AppResult};

/// 原子写文本文件：在目标目录以排他、不可预测的名字创建临时文件，写入后原子替换目标。
///
/// 权限语义对齐 TS `writeFileAtomic`（hookConfigUtils.js）：
/// - 未显式指定 `mode` 时，临时文件以 0666 创建、由内核套用进程 umask——与 TS
///   `fs.writeFile` 的默认创建一致；不在写入后统一 chmod，也不读取/修改全局 umask。
/// - 显式 `mode` 在写入后精确 chmod 生效，与 TS `options.mode` 分支一致。
///
/// 临时文件名不可预测且排他创建，失败路径尽力清理；阻塞文件操作放入 blocking 线程池。
pub(super) async fn write_file_atomic(
    target: &Path,
    content: &str,
    mode: Option<u32>,
) -> AppResult<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).await?;
    let dir = dir.to_path_buf();
    let target = target.to_path_buf();
    let content = content.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || write_file_atomic_sync(&dir, &target, &content, mode))
        .await
        .map_err(|error| AppError::system(format!("join atomic file writer: {error}")))?
}

/// [`write_file_atomic`] 的同步核心，独立出来供权限语义测试在子进程中直接驱动。
fn write_file_atomic_sync(
    dir: &Path,
    target: &Path,
    content: &[u8],
    mode: Option<u32>,
) -> AppResult<()> {
    std::fs::create_dir_all(dir)?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hook-config".to_string());
    // 排他 + 不可预测临时名；名字碰撞时换名重试，其他错误立即传播。
    let mut name_collision = None;
    for _ in 0..8 {
        let temporary = dir.join(format!(
            ".{file_name}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let write_result = open_exclusive(&temporary).and_then(|mut file| {
            let result = (|| -> std::io::Result<()> {
                file.write_all(content)?;
                file.flush()?;
                if let Some(mode) = mode {
                    set_mode(&file, mode)
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                }
                file.sync_all()?;
                drop(file);
                std::fs::rename(&temporary, target)
            })();
            if result.is_err() {
                // 清理失败路径上的临时文件；清理本身的失败不覆盖原始错误。
                drop(std::fs::remove_file(&temporary));
            }
            result
        });
        match write_result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                name_collision = Some(error);
            }
            Err(error) => {
                return Err(AppError::system(format!(
                    "atomic write to {} failed: {error}",
                    target.display()
                )));
            }
        }
    }
    Err(AppError::system(format!(
        "atomic write to {} could not create a unique temporary file: {}",
        target.display(),
        name_collision
            .map(|error| error.to_string())
            .unwrap_or_default()
    )))
}

/// 以 0666 创建权限排他打开临时文件；unix 上由内核套用进程 umask（对齐
/// `fs.writeFile` 默认），非 unix 无 mode 概念、退化为普通排他创建。
#[cfg(unix)]
fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o666)
        .open(path)
}
#[cfg(not(unix))]
fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

/// 原子写 JSON 文件 (对齐 nuwax writeJsonFileAtomic: pretty + 末尾换行)。
pub(super) async fn write_json_file_atomic(target: &Path, data: &Value) -> AppResult<()> {
    let mut s = serde_json::to_string_pretty(data)
        .map_err(|e| AppError::system(format!("serialize json: {e}")))?;
    s.push('\n');
    write_file_atomic(target, &s, None).await
}

/// 删除文件、目录或符号链接；不存在视为成功，其他 I/O 错误立即返回。
pub(super) async fn remove_path_if_exists(path: &Path) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).await?;
    } else {
        fs::remove_file(path).await?;
    }
    Ok(())
}

/// 设置文件权限 (unix only; 0o755 等)。
#[cfg(unix)]
fn set_mode(file: &std::fs::File, mode: u32) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _mode: u32) -> AppResult<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod umask_tests {
    use super::write_file_atomic_sync;

    const CHILD_TARGET_ENV: &str = "AB_ATOMIC_WRITE_CHILD_TARGET";

    fn mode_string(path: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).expect("stat written file");
        format!("{:04o}", metadata.permissions().mode() & 0o777)
    }

    /// 子进程分支：写默认与显式 mode 两个文件并打印权限，供父进程断言。
    /// umask 由父进程通过 `sh -c 'umask X && exec …'` 设置，不在测试进程内修改。
    fn child_write() {
        let Ok(root) = std::env::var(CHILD_TARGET_ENV) else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let default_file = root.join("default.json");
        let explicit_file = root.join("explicit.json");
        write_file_atomic_sync(&root, &default_file, b"{}", None).expect("default write");
        write_file_atomic_sync(&root, &explicit_file, b"{}", Some(0o640)).expect("explicit write");
        println!(
            "default={} explicit={}",
            mode_string(&default_file),
            mode_string(&explicit_file)
        );
    }

    #[test]
    fn atomic_write_permissions_follow_umask_and_explicit_mode_is_exact() {
        // 子进程分支：完成写入后直接退出，不进入父进程的 spawn 循环。
        if std::env::var(CHILD_TARGET_ENV).is_ok() {
            child_write();
            std::process::exit(0);
        }
        // 通过 `sh -c 'umask X && exec $exe …'` 在隔离子进程设置 umask，
        // 避免污染并行 nextest 进程的进程级 umask。
        for (umask, default_mode) in [("022", "0644"), ("077", "0600"), ("002", "0664")] {
            let root = std::env::temp_dir()
                .join(format!("ab-atomic-umask-{umask}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("create temp root");
            let exe = std::env::current_exe().expect("current test binary");
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("umask {umask} && exec \"$@\""))
                // "$@" 从 $1 开始展开；占位 $0 后测试二进制是第一个实参。
                .arg("umask-child")
                .arg(&exe)
                .args([
                    "--exact",
                    "service::agent_hooks::io_util::umask_tests::atomic_write_permissions_follow_umask_and_explicit_mode_is_exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_TARGET_ENV, &root)
                .output()
                .expect("spawn umask child");
            assert!(
                output.status.success(),
                "umask {umask} child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            // libtest 在 --nocapture 下会给输出加 "test <name> ... " 前缀，
            // 因此按子串定位报告而非行首。
            let modes: Vec<&str> = stdout
                .find("default=")
                .map(|start| &stdout[start + "default=".len()..])
                .map(|rest| {
                    rest.split_whitespace()
                        .map(|token| token.trim_start_matches("explicit="))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    panic!(
                        "umask {umask} child must print a permission report; stdout={stdout:?} stderr={:?}",
                        String::from_utf8_lossy(&output.stderr)
                    )
                });
            assert_eq!(modes[0], default_mode, "umask {umask} default creation");
            // 显式 mode 精确生效，不随 umask 变化。
            assert_eq!(modes[1], "0640", "umask {umask} explicit mode");
            drop(std::fs::remove_dir_all(&root));
        }
    }
}

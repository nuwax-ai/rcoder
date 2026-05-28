//! Auto-Reload 热重载机制
//!
//! 检测 Agent 二进制文件变化并自动触发重载。
//! 主要用于 DevComputer 调试场景，开发者编译新 agent 后无需手动重启。
//!
//! # 工作原理
//!
//! 1. **resolve_agent_binary()**: 根据 command 解析出真实的二进制路径
//! 2. **take_snapshot()**: 读取文件的 mtime 和 size 创建快照
//! 3. **is_changed()**: 比较新旧快照，判断文件是否变化
//! 4. **wait_for_stability()**: 防止加载编译中的二进制，等待 mtime 稳定
//!
//! # Stability Check
//!
//! 高速编译场景下，文件可能在写入过程中被检测到变化。
//! Stability check 通过多次采样 mtime 来确保文件已写入完毕：
//! - 每隔 `stability_check_ms` 检查一次
//! - 连续 `stability_retries` 次 mtime 不变才算通过

use std::path::{Path, PathBuf};
use std::time::Duration;

use shared_types::AutoReloadConfig;
use shared_types::AgentBinarySnapshot;
use tracing::{debug, info, warn};

/// 解析 agent 命令对应的二进制文件路径
///
/// 对于绝对/相对路径直接返回；对于 PATH 中的命令使用 `which` 解析。
///
/// # Arguments
/// * `command` - agent 启动命令（如 "codex-acp", "./my-agent"）
/// * `working_dir` - 工作目录，用于解析相对路径
///
/// # Returns
/// 解析后的绝对路径，如果找不到则返回 None
pub fn resolve_agent_binary(command: &str, working_dir: &Path) -> Option<PathBuf> {
    let path = Path::new(command);

    // 如果是绝对路径，直接返回
    if path.is_absolute() {
        return if path.exists() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }

    // 如果包含路径分隔符（./my-agent 或 ../my-agent），相对于 working_dir 解析
    if command.contains('/') || command.contains('\\') {
        let resolved = working_dir.join(path);
        return if resolved.exists() {
            Some(resolved)
        } else {
            None
        };
    }

    // 否则在 PATH 中查找
    which::which(command).ok()
}

/// 创建二进制文件快照
///
/// 读取文件的 metadata（mtime + size），用于后续变化检测。
pub fn take_snapshot(path: &Path) -> Option<AgentBinarySnapshot> {
    AgentBinarySnapshot::from_path(path)
}

/// 检查二进制文件是否发生变化
///
/// 比较新快照与旧快照，如果 mtime 或 size 不同则认为已变化。
///
/// # Arguments
/// * `old_snapshot` - 之前的快照（如果为 None，视为已变化）
/// * `new_snapshot` - 当前快照
pub fn is_changed(
    old_snapshot: &Option<AgentBinarySnapshot>,
    new_snapshot: &AgentBinarySnapshot,
) -> bool {
    match old_snapshot {
        None => true, // 首次，视为已变化
        Some(old) => !old.is_same_as(new_snapshot),
    }
}

/// 等待二进制文件稳定（防止加载编译中的二进制）
///
/// 在 `config.stability_check_ms` 间隔内多次检查 mtime，
/// 连续 `config.stability_retries` 次不变才算稳定。
///
/// # Returns
/// - `Ok(true)` — 文件已稳定
/// - `Ok(false)` — 达到最大重试次数仍未稳定
pub async fn wait_for_stability(
    path: &Path,
    config: &AutoReloadConfig,
) -> Result<bool, std::io::Error> {
    if config.force {
        debug!("[auto_reload] force=true, skipping stability check");
        return Ok(true);
    }

    let interval = Duration::from_millis(config.stability_check_ms);
    let max_retries = config.stability_retries;

    let mut last_snapshot = take_snapshot(path);
    let mut stable_count = 0u32;

    for attempt in 1..=max_retries {
        tokio::time::sleep(interval).await;

        let current_snapshot = take_snapshot(path);

        match (&last_snapshot, &current_snapshot) {
            (Some(prev), Some(curr)) if prev.is_same_as(curr) => {
                stable_count += 1;
                debug!(
                    "[auto_reload] stability check {}/{}: stable",
                    attempt, max_retries
                );
            }
            _ => {
                stable_count = 0;
                debug!(
                    "[auto_reload] stability check {}/{}: file still changing",
                    attempt, max_retries
                );
            }
        }

        last_snapshot = current_snapshot;

        if stable_count >= max_retries {
            info!(
                "[auto_reload] binary stabilized after {} checks",
                attempt
            );
            return Ok(true);
        }
    }

    warn!(
        "[auto_reload] binary did not stabilize after {} retries",
        max_retries
    );
    Ok(false)
}

/// 完整的 auto-reload 检测流程
///
/// 1. 解析二进制路径
/// 2. 创建新快照
/// 3. 比较是否有变化
/// 4. 如果有变化，等待稳定性检查
/// 5. 返回是否需要重载
///
/// # Arguments
/// * `command` - agent 启动命令
/// * `working_dir` - 工作目录
/// * `old_snapshot` - 当前存储的快照（来自 ProjectAndAgentInfo）
/// * `config` - auto-reload 配置
///
/// # Returns
/// `Some(new_snapshot)` 表示需要重载，`None` 表示无需重载
pub async fn check_and_wait_for_reload(
    command: &str,
    working_dir: &Path,
    old_snapshot: &Option<AgentBinarySnapshot>,
    config: &AutoReloadConfig,
) -> Option<AgentBinarySnapshot> {
    if !config.enabled {
        return None;
    }

    // 1. 解析二进制路径
    let binary_path = resolve_agent_binary(command, working_dir)?;

    // 2. 创建新快照
    let new_snapshot = take_snapshot(&binary_path)?;

    // 3. 比较是否有变化
    if !is_changed(old_snapshot, &new_snapshot) {
        debug!("[auto_reload] binary unchanged, skipping reload");
        return None;
    }

    info!(
        "[auto_reload] binary changed: path={}, mtime={}, size={}",
        new_snapshot.path.display(),
        new_snapshot.modified_secs,
        new_snapshot.size_bytes
    );

    // 4. 等待稳定性检查
    match wait_for_stability(&binary_path, config).await {
        Ok(true) => {
            // Re-take snapshot after stabilization (file may have changed during wait)
            let final_snapshot = take_snapshot(&binary_path)?;
            info!("[auto_reload] reload triggered");
            Some(final_snapshot)
        }
        Ok(false) => {
            warn!("[auto_reload] binary did not stabilize, skipping reload");
            None
        }
        Err(e) => {
            warn!("[auto_reload] stability check failed: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_resolve_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "fake binary").unwrap();

        let result = resolve_agent_binary(bin_path.to_str().unwrap(), tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn test_resolve_relative_path() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "fake binary").unwrap();

        let result = resolve_agent_binary("./my-agent", tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn test_resolve_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_agent_binary("./nonexistent-agent", tmp.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_snapshot_and_compare() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "version 1").unwrap();

        let snap1 = take_snapshot(&bin_path).unwrap();
        let snap2 = take_snapshot(&bin_path).unwrap();

        assert!(snap1.is_same_as(&snap2));
        assert!(!is_changed(&Some(snap1.clone()), &snap2));

        // Modify the file
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&bin_path, "version 2 with more content").unwrap();

        let snap3 = take_snapshot(&bin_path).unwrap();
        assert!(!snap1.is_same_as(&snap3));
        assert!(is_changed(&Some(snap1), &snap3));
    }

    #[test]
    fn test_is_changed_with_none_old() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "content").unwrap();

        let snap = take_snapshot(&bin_path).unwrap();
        assert!(is_changed(&None, &snap));
    }

    #[tokio::test]
    async fn test_stability_check_force() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "content").unwrap();

        let config = AutoReloadConfig {
            enabled: true,
            stability_check_ms: 10,
            stability_retries: 3,
            force: true,
        };

        let result = wait_for_stability(&bin_path, &config).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_stability_check_already_stable() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "content").unwrap();

        let config = AutoReloadConfig {
            enabled: true,
            stability_check_ms: 10,
            stability_retries: 2,
            force: false,
        };

        let result = wait_for_stability(&bin_path, &config).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_check_and_wait_for_reload_disabled() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "content").unwrap();

        let config = AutoReloadConfig::disabled();
        let result = check_and_wait_for_reload(
            bin_path.to_str().unwrap(),
            tmp.path(),
            &None,
            &config,
        )
        .await;
        assert!(result.is_none());
    }
}

//! 配置文件监控模块
//!
//! 本模块使用 `notify` crate 实现配置文件的实时监控和热更新。
//! 当 `config.yml` 文件被修改时,自动重新加载 API Key 配置,无需重启服务。
//!
//! # 监听通路
//!
//! 三层互补，覆盖两类编辑方式与容器单文件 bind 场景：
//!
//! 1. **文件 inode watch**：同 inode 原地写（`echo >>` / `tee` / 程序直写）。
//!    inotify 语义下目录 watch **收不到**子文件的内容修改事件，必须 watch
//!    文件本身；单文件 bind mount 两侧共享同一 inode，宿主侧原地写的事件
//!    会投递到容器内对该 inode 的 watch。
//! 2. **父目录 watch**：编辑器原子保存（write-to-temp + rename）产生的
//!    目录项变化（IN_MOVED_TO / Create）。
//! 3. **低频轮询兜底**（默认 3s，`RCODER_CONFIG_POLL_SECS` 可调，0 禁用）：
//!    内容 hash 快照比对（mtime 在跨虚拟化文件系统下可能被属性缓存拖后，
//!    内容读取没有该问题）。事件通路在异构挂载/传播特性下仍可能丢事件，
//!    轮询保证修改后有界生效。
//!
//! 已知边界（bind mount 固有，非 watcher 可解）：宿主以 rename 语义编辑
//!（`sed -i` / vim 原子保存）时，容器内单文件 bind 仍指向旧 inode——内容
//! 都不会更新，事件与轮询均不触发，需原地写方式或重启容器重新绑定。
//!
//! # 其他特性
//!
//! - 配置验证（enabled 且空 key 拒绝，沿用旧配置，不 panic、不放行）
//! - 线程安全更新（ArcSwap 原子换）
//! - 相对路径（容器形态默认 `config.yml` / `RCODER_CONFIG_FILE`）注册前
//!   归一为绝对路径，消除空 parent 目录与 cwd 依赖漂移

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{ApiKeyAuthConfig, load_api_key_config_from_file};

/// 轮询兜底默认间隔（秒）；`RCODER_CONFIG_POLL_SECS` 可覆盖（0 = 禁用）。
const DEFAULT_POLL_INTERVAL_SECS: u64 = 3;

/// 配置文件监控器
///
/// 内部持有 `RecommendedWatcher` 以保持文件监控活跃。
/// 一旦 ConfigWatcher 被 drop,文件监控将停止。
pub struct ConfigWatcher {
    /// 文件系统监控器(必须保持存活)
    _watcher: RecommendedWatcher,
}

/// 相对路径 → 绝对路径（相对当前工作目录拼接）。注册 watch、读取与事件
/// 过滤共用同一份规范化结果；绝对路径原样保留（不 canonicalize——symlink
/// 与 bind mount 场景下按调用者给定路径稳定工作）。
fn normalize_config_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().context("resolve config path working directory")?;
    Ok(cwd.join(path))
}

/// 轮询间隔：env 覆盖（`RCODER_CONFIG_POLL_SECS`，0 禁用），默认 3s。
fn poll_interval_from_env() -> Duration {
    let secs = std::env::var("RCODER_CONFIG_POLL_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    Duration::from_secs(secs)
}

impl ConfigWatcher {
    /// 创建新的配置监控器（默认轮询间隔，见模块文档）。
    pub fn new(
        config_path: PathBuf,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    ) -> Result<Self> {
        Self::with_poll_interval(config_path, api_key_config, poll_interval_from_env())
    }

    /// 创建监控器并显式指定轮询兜底间隔（`Duration::ZERO` 禁用轮询；
    /// 测试用短间隔确定性覆盖轮询路径）。
    pub fn with_poll_interval(
        config_path: PathBuf,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
        poll_interval: Duration,
    ) -> Result<Self> {
        let config_path = normalize_config_path(&config_path)?;
        let (tx, mut rx) = mpsc::channel(100);

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res
                && let Err(e) = tx.blocking_send(event)
            {
                warn!("config watcher event send failed (consumer gone): {e}");
            }
        })?;

        // 双 watch：
        // - 文件本身（同 inode 原地写的内容修改事件只在文件 inode watch 上
        //   投递，目录 watch 收不到——这是原实现热加载失效的直接根因）；
        // - 父目录（原子保存 write-to-temp + rename 的目录项事件）。
        watcher.watch(&config_path, RecursiveMode::NonRecursive)?;
        if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }
        info!(
            "📁 [CONFIG_WATCHER] Watching config file: {:?} (file + parent dir) with {:?} poll fallback",
            config_path, poll_interval
        );

        let file_name = config_path.file_name().map(|n| n.to_os_string());
        let event_config_path = config_path.clone();
        let event_api_key_config = Arc::clone(&api_key_config);
        // 事件通路：过滤目标文件事件（文件 watch 的事件 path 即目标文件；
        // 目录 watch 的事件按 file_name 过滤出目标），交公共重载入口。
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Some(ref name) = file_name {
                    let event_matches = event.paths.iter().any(|p| {
                        p.file_name()
                            .map(|n| n == name.as_os_str())
                            .unwrap_or(false)
                    });
                    if !event_matches {
                        continue;
                    }
                }
                // EventKind::Any：macOS fsevent 默认 imprecise 模式把所有
                // 变化都报告为 Any（无细粒度 kind）；目标文件已按路径过滤，
                // Any 也必须触发重载。
                if matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
                ) {
                    reload_with_retry(&event_config_path, Arc::clone(&event_api_key_config)).await;
                }
            }
        });

        // 轮询兜底通路：内容 hash 快照比对（事件丢失/异构挂载传播特性差异
        // 时保证有界生效；mtime 在跨虚拟化文件系统下可能被属性缓存拖后）。
        // 文件暂时不可读（读取失败）保持快照不变，待恢复后按变化处理；文件
        // 被 rename 替换（bind 滞留旧 inode）时内容不变，轮询正确地不触发。
        if !poll_interval.is_zero() {
            let poll_config_path = config_path.clone();
            let poll_api_key_config = Arc::clone(&api_key_config);
            // 基线快照在构造同步段捕获（而非任务首次调度时）——构造与
            // 首个 tick 之间发生的修改必须被检测，不能被误当作基线。
            let mut snapshot = file_fingerprint(&config_path);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(poll_interval).await;
                    let current = file_fingerprint(&poll_config_path);
                    if current != snapshot {
                        info!(
                            "[CONFIG_WATCHER] poll detected config change: {:?} -> {:?}",
                            snapshot, current
                        );
                        snapshot = current;
                        reload_with_retry(&poll_config_path, Arc::clone(&poll_api_key_config))
                            .await;
                    }
                }
            });
        }

        Ok(Self { _watcher: watcher })
    }

    /// 重新加载配置（使用 ArcSwap 无锁更新）。
    /// 校验失败（enabled 且空 key / 解析失败）时不写入 ArcSwap——调用方与
    /// 调用方旧配置保持生效，不 panic、不放行。
    async fn reload_config(
        config_path: &Path,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    ) -> Result<()> {
        match load_api_key_config_from_file(config_path) {
            Ok(new_config) => {
                // 验证配置有效性
                if new_config.enabled && new_config.api_key.trim().is_empty() {
                    error!("[CONFIG_WATCHER] API Key is empty");
                    return Err(anyhow::anyhow!("API Key cannot be empty string"));
                }

                // 🚀 使用 ArcSwap 原子更新配置（无锁，不阻塞读取）
                let old_config = api_key_config.load();
                let old_enabled = old_config.enabled;
                let key_changed = old_config.api_key != new_config.api_key;

                // 提前保存新配置状态（用于日志）
                let new_enabled = new_config.enabled;

                // 原子替换配置（移动所有权，避免 clone）
                api_key_config.store(Arc::new(new_config));

                // 记录配置变更
                if old_enabled != new_enabled {
                    info!(
                        "🔄 [CONFIG_WATCHER] API Key auth status updated: {} -> {}",
                        old_enabled, new_enabled
                    );
                }

                if key_changed {
                    info!("[CONFIG_WATCHER] API Key updated");
                }

                if !old_enabled && !new_enabled && !key_changed {
                    // 配置未实际变化，不记录日志
                    return Ok(());
                }

                info!("[CONFIG_WATCHER] Config update succeeded");
                Ok(())
            }
            Err(e) => {
                error!("[CONFIG_WATCHER] Config file reload failed: {}", e);
                Err(e)
            }
        }
    }
}

/// 文件指纹（内容 hash）：原地写必然变化；读不到时返回 None（与任何
/// Some 互不相等——文件短暂消失再出现会按变化处理一次，幂等重载无害）。
/// 用内容而非 mtime/size：跨虚拟化文件系统（virtiofs 等）与异构 bind
/// 传播下，容器内看到的 mtime 可能被属性缓存拖后，内容读取没有该问题。
fn file_fingerprint(path: &Path) -> Option<u64> {
    let content = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash_slice(&content, &mut hasher);
    Some(std::hash::Hasher::finish(&hasher))
}

/// 带重试的重载入口（事件与轮询共用）：编辑器/写入方可能尚未写完整，
/// 小间隔重试后再放弃（失败保留旧配置）。
async fn reload_with_retry(config_path: &Path, api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>) {
    let max_retries = 3;
    let mut retry_delay = Duration::from_millis(100);
    for attempt in 1..=max_retries {
        tokio::time::sleep(retry_delay).await;
        match ConfigWatcher::reload_config(config_path, Arc::clone(&api_key_config)).await {
            Ok(_) => break,
            Err(e) => {
                if attempt == max_retries {
                    warn!(
                        "[CONFIG_WATCHER] config reload failed after {} attempts: {}",
                        max_retries, e
                    );
                } else {
                    warn!(
                        "[CONFIG_WATCHER] config reload attempt {}/{} failed: {}, retrying in {}ms",
                        attempt,
                        max_retries,
                        e,
                        retry_delay.as_millis()
                    );
                    retry_delay *= 2; // 指数退避
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_config(path: &Path, enabled: bool, key: &str) {
        fs::write(
            path,
            format!("\napi_key_auth:\n  enabled: {enabled}\n  api_key: \"{key}\"\n"),
        )
        .unwrap();
    }

    async fn wait_until(
        api_key_config: &Arc<ArcSwap<ApiKeyAuthConfig>>,
        expect_enabled: bool,
        expect_key: &str,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let current = api_key_config.load();
            if current.enabled == expect_enabled && current.api_key == expect_key {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "hot reload never took effect: enabled={} key={}",
                current.enabled,
                current.api_key
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_config_watcher_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_config(&config_path, false, "sk-test123");

        let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: false,
            api_key: "sk-test123".to_string(),
        }));

        let watcher =
            ConfigWatcher::with_poll_interval(config_path, api_key_config, Duration::ZERO);
        assert!(watcher.is_ok());
    }

    /// 反例（修复锚点）：同 inode 原地写（fs::write truncate+write，等价
    /// `echo >`）必须触发热加载——旧实现只 watch 父目录，而 inotify 目录
    /// watch 收不到子文件的内容修改事件，此用例在修复前超时失败。
    /// 生产形态事件+轮询双通路常开（Linux inotify 事件先到；本地 macOS
    /// fsevent 高负载下由轮询保底送达），断言的验收行为不变。
    #[tokio::test]
    async fn in_place_overwrite_triggers_hot_reload() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_config(&config_path, false, "sk-old");

        let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: false,
            api_key: "sk-old".to_string(),
        }));
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(150),
        )
        .unwrap();

        // 同 inode 原地覆盖（不 rename、不换 inode）
        write_config(&config_path, true, "sk-new");
        wait_until(&api_key_config, true, "sk-new").await;
    }

    /// 轮询兜底通路：内容变化后一个轮询周期内生效（事件通路不可用
    /// 或丢事件时的保底）。
    #[tokio::test]
    async fn poll_fallback_reloads_after_interval() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_config(&config_path, false, "sk-old");

        let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: false,
            api_key: "sk-old".to_string(),
        }));
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(150),
        )
        .unwrap();

        // 等首个轮询周期建立基线快照后再改文件
        tokio::time::sleep(Duration::from_millis(300)).await;
        write_config(&config_path, true, "sk-poll");
        wait_until(&api_key_config, true, "sk-poll").await;
    }

    /// 非法配置（enabled 且空 key）：打错误、沿用旧配置（不 panic、不放行
    /// ——验收标准 2）。随后恢复合法配置仍可继续热加载。
    #[tokio::test]
    async fn invalid_config_keeps_old_and_recovers() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_config(&config_path, true, "sk-valid");

        let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: true,
            api_key: "sk-valid".to_string(),
        }));
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(150),
        )
        .unwrap();

        // 非法：enabled + 空 key
        write_config(&config_path, true, "");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let current = api_key_config.load();
        assert!(
            current.enabled && current.api_key == "sk-valid",
            "invalid config must keep the old one: enabled={} key={}",
            current.enabled,
            current.api_key
        );

        // 恢复合法配置：热加载继续
        write_config(&config_path, true, "sk-rotated");
        wait_until(&api_key_config, true, "sk-rotated").await;
    }

    /// 相对路径（容器形态默认 `config.yml` / `RCODER_CONFIG_FILE` 相对形态）
    /// 注册前归一为绝对路径；绝对路径原样保留。
    #[test]
    fn normalize_config_path_resolves_relative_and_keeps_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let relative = normalize_config_path(Path::new("config.yml")).unwrap();
        assert_eq!(relative, cwd.join("config.yml"));

        let absolute = normalize_config_path(Path::new("/app/config.yml")).unwrap();
        assert_eq!(absolute, Path::new("/app/config.yml"));
    }
}

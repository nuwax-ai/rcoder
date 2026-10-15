//! 配置文件监控模块
//!
//! 本模块使用 `notify` crate 实现配置文件的实时监控和热更新。
//! 当 `config.yml` 文件被修改时,自动重新加载 API Key 配置,无需重启服务。
//!
//! # 监听通路
//!
//! 三层互补，覆盖两类编辑方式与容器单文件 bind 场景：
//!
//! 1. **文件 watch**：单文件 bind mount 两侧共享同一 inode，宿主侧对 bind
//!    源文件的原地写（`echo >>` / `tee` / 程序直写）会投递到容器内对该
//!    inode 的监听。容器部署实测：只监听父目录时此类写法零事件——宿主
//!    修改走的是宿主侧目录路径，而非容器内被监听的目录项（inotify(7)：
//!    目录监听不覆盖通过该目录之外的其他路径进行的操作）。
//! 2. **父目录 watch**：编辑器原子保存（write-to-temp + rename）产生的
//!    目录项变化（IN_MOVED_TO → Create/Modify(Name)）。
//! 3. **低频轮询兜底**（默认 3s，`RCODER_CONFIG_POLL_SECS` 可调，0 禁用）：
//!    内容快照比对。事件通路在异构挂载/传播特性差异下仍可能丢事件，
//!    轮询在新内容对容器可见后持续重试；实测跨虚拟化
//!    文件系统的内容可见性延迟远小于 mtime 属性缓存的抖动，故用内容
//!    而非 mtime+size 做指纹。
//!
//! # 一致性规则
//!
//! - **单一重载执行者**：事件与轮询只负责触发，读取、解析、提交在同一个
//!   串行任务中按序执行——不存在并发提交导致的旧值倒序覆盖；
//! - **同一份字节用于指纹与解析**；
//! - **成功提交后才更新已应用指纹**：重载失败（文件半成品/暂时不可读）
//!   保留旧指纹，下一次触发自动重试；
//! - **启动对账**：构造后立即按当前文件内容对账一次——启动读取配置与
//!   watcher 创建之间发生的修改不会因被当作基线而永久遗漏；
//! - **半成品不改变运行配置**：`api_key_auth` 段缺失/非法时保留旧配置
//!   并报错（关闭鉴权必须显式 `enabled: false`）；
//! - 环境变量覆盖（`RCODER_API_KEY_ENABLED`/`RCODER_API_KEY`）与启动
//!   加载共用同一规则（见 `config::apply_api_key_env_overrides`），热修改
//!   不会静默覆盖环境变量指定的鉴权状态。
//!
//! # 已知边界（bind mount 固有）
//!
//! 宿主以 rename 语义编辑（`sed -i` / vim 原子保存）时，容器内单文件
//! bind 仍指向旧 inode——**新内容尚未进入容器的挂载视图**，watcher 与
//! 轮询读到的都是旧内容，热加载无从生效；需原地写方式或重启容器重新
//! 绑定。旧 inode 自身的属性事件仍可能触发一次重载，结果与旧内容一致。
//!
//! # 其他特性
//!
//! - 配置验证（enabled 且空 key 拒绝，沿用旧配置，不 panic、不放行）
//! - 线程安全更新（ArcSwap 原子换）
//! - 相对路径（容器形态默认 `config.yml` / `RCODER_CONFIG_FILE`）注册前
//!   归一为绝对路径，消除空 parent 目录与 cwd 依赖漂移
//! - watch 注册失败不放弃热加载：轮询兜底在则降级继续（warn 记录原因）

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{ApiKeyAuthConfig, parse_api_key_config};

/// 轮询兜底默认间隔（秒）；`RCODER_CONFIG_POLL_SECS` 可覆盖（0 = 禁用）。
/// 本地正常 I/O 下，新内容可读后通常在一个周期内被检测；挂载传播、
/// 读取失败或调度延迟不属于固定的时间保证。
const DEFAULT_POLL_INTERVAL_SECS: u64 = 3;

/// 配置文件监控器。
///
/// 持有 native watcher（可能为 `None`：降级纯轮询）与重载任务句柄；
/// drop 撤销发布权限并取消任务。已经开始的同步读取可能稍后才返回，
/// 但其结果不得再提交；不会等待磁盘 I/O 完成才允许关停。
pub struct ConfigWatcher {
    _watcher: Option<RecommendedWatcher>,
    _reloader: tokio::task::JoinHandle<()>,
    updates_enabled: Arc<Mutex<bool>>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // abort is cooperative. Revoke publication under the same short lock
        // used by ArcSwap commits before cancelling an in-flight read/parse.
        *self.updates_enabled.lock() = false;
        self._reloader.abort();
    }
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
        Self::build(config_path, api_key_config, poll_interval, true)
    }

    /// 仅轮询（无 native watcher）。既用于 watcher 构造失败时的降级，
    /// 也供测试确定性覆盖纯轮询路径。
    pub fn poll_only(
        config_path: PathBuf,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
        poll_interval: Duration,
    ) -> Result<Self> {
        anyhow::ensure!(
            !poll_interval.is_zero(),
            "poll-only watcher requires a non-zero poll interval"
        );
        Self::build(config_path, api_key_config, poll_interval, false)
    }

    fn build(
        config_path: PathBuf,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
        poll_interval: Duration,
        use_native_watcher: bool,
    ) -> Result<Self> {
        let config_path = normalize_config_path(&config_path)?;
        let mut event_rx = None;
        let watcher = if use_native_watcher {
            match Self::spawn_watches(&config_path) {
                Ok((watcher, rx)) => {
                    event_rx = Some(rx);
                    Some(watcher)
                }
                Err(error) => {
                    // 注册失败不放弃热加载：轮询兜底在则降级继续；轮询也
                    // 禁用时如实报错（上层 warn 降级，与既有装配语义一致）。
                    if poll_interval.is_zero() {
                        return Err(error.context("config file watcher unavailable"));
                    }
                    warn!(
                        "📁 [CONFIG_WATCHER] native watcher unavailable, degrading to poll-only: {error:#}"
                    );
                    None
                }
            }
        } else {
            None
        };
        info!(
            "📁 [CONFIG_WATCHER] Watching config file: {:?} ({}, poll fallback {:?})",
            config_path,
            if watcher.is_some() {
                "file + parent dir"
            } else {
                "poll-only"
            },
            poll_interval
        );

        let updates_enabled = Arc::new(Mutex::new(true));
        let reloader = tokio::spawn(reload_loop(
            config_path,
            event_rx,
            poll_interval,
            api_key_config,
            Arc::clone(&updates_enabled),
        ));

        Ok(Self {
            _watcher: watcher,
            _reloader: reloader,
            updates_enabled,
        })
    }

    /// 文件 + 父目录双监听。单侧失败降级为另一侧（warn）——文件监听覆盖
    /// 同 inode 原地写，目录监听覆盖原子保存 rename；任一侧注册成功即保留
    /// native 通路（事件接收端一并返回），双侧失败向上返回错误由调用方
    /// 决定降级。通知只表示需要重读当前文件，合并连续事件而不阻塞
    /// native 回调；监听错误和重扫请求同样触发对账。
    fn spawn_watches(config_path: &Path) -> Result<(RecommendedWatcher, mpsc::Receiver<()>)> {
        let (tx, rx) = mpsc::channel(1);
        let file_name = config_path.file_name().map(|name| name.to_os_string());
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let reload = match res {
                Ok(event) => event_targets(&event, file_name.as_deref()),
                Err(error) => {
                    warn!("[CONFIG_WATCHER] watcher error event: {error}");
                    true
                }
            };
            if reload {
                match tx.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
                        // A queued signal already covers the latest contents.
                    }
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        // The reloader has stopped; no further work is owned.
                    }
                }
            }
        })?;
        let mut registered = false;
        match watcher.watch(config_path, RecursiveMode::NonRecursive) {
            Ok(()) => registered = true,
            Err(error) => warn!(
                "[CONFIG_WATCHER] file watch unavailable ({}): {error}",
                config_path.display()
            ),
        }
        if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            match watcher.watch(parent, RecursiveMode::NonRecursive) {
                Ok(()) => registered = true,
                Err(error) => warn!(
                    "[CONFIG_WATCHER] parent dir watch unavailable ({}): {error}",
                    parent.display()
                ),
            }
        }
        anyhow::ensure!(registered, "neither file nor parent dir watch registered");
        Ok((watcher, rx))
    }
}

/// 单一重载执行者：事件与轮询只是触发源，读取、解析、提交全部在此串行
/// 执行（见模块「一致性规则」）。
async fn reload_loop(
    config_path: PathBuf,
    mut event_rx: Option<mpsc::Receiver<()>>,
    poll_interval: Duration,
    api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    updates_enabled: Arc<Mutex<bool>>,
) {
    let mut applied: Option<Vec<u8>> = None;
    // 启动对账：构造与启动配置读取之间发生的修改在此补齐（applied 为空，
    // 首次必然按当前文件内容对账；与启动值一致时静默无日志）。
    reload_with_retry(
        &config_path,
        &mut applied,
        &api_key_config,
        &updates_enabled,
    )
    .await;

    let poll_enabled = !poll_interval.is_zero();
    let mut ticker = tokio::time::interval(poll_interval.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // 触发源：已过滤、合并的通知或轮询 tick。事件源枯竭后自然
        // 退化为纯轮询，所有读取与提交仍由此任务串行执行。
        let triggered = match event_rx.as_mut() {
            Some(rx) => {
                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(()) => true,
                        None => {
                            // 通道关闭：watcher 已 drop，停用事件源。
                            event_rx = None;
                            false
                        }
                    },
                    _ = ticker.tick(), if poll_enabled => true,
                }
            }
            None => {
                if !poll_enabled {
                    // 无事件源且轮询禁用：构造期已拒绝该组合，防御退出。
                    break;
                }
                ticker.tick().await;
                true
            }
        };
        if triggered {
            reload_with_retry(
                &config_path,
                &mut applied,
                &api_key_config,
                &updates_enabled,
            )
            .await;
        }
    }
}

/// 事件是否指向目标配置文件且属于修改/创建类。`EventKind::Any`：个别
/// 后端/事件形态不提供细粒度分类（无 kind 信息），目标已按路径过滤，
/// 一并接受。
fn event_targets(event: &Event, file_name: Option<&std::ffi::OsStr>) -> bool {
    if event.need_rescan() {
        return true;
    }
    if let Some(name) = file_name {
        let matches_name = event
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == name));
        if !matches_name {
            return false;
        }
    }
    matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
    )
}

/// 一次重载尝试（带小间隔重试覆盖写一半窗口）：读文件字节 → 与已应用
/// 指纹比对（相同即静默跳过）→ 解析（缺段/非法保留旧值）→ 提交 →
/// **成功后**才更新已应用指纹。
async fn reload_with_retry(
    config_path: &Path,
    applied: &mut Option<Vec<u8>>,
    api_key_config: &Arc<ArcSwap<ApiKeyAuthConfig>>,
    updates_enabled: &Mutex<bool>,
) {
    let max_retries = 3;
    let mut retry_delay = Duration::from_millis(100);
    for attempt in 1..=max_retries {
        tokio::time::sleep(retry_delay).await;
        if !*updates_enabled.lock() {
            return;
        }
        let bytes = match std::fs::read(config_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                // 文件暂时不可读（写入方正在替换等）：不更新已应用指纹，
                // 下一次触发重试。
                if attempt == max_retries {
                    warn!(
                        "[CONFIG_WATCHER] config file unreadable ({}): {error}",
                        config_path.display()
                    );
                }
                retry_delay *= 2;
                continue;
            }
        };
        if applied.as_deref() == Some(bytes.as_slice()) {
            return; // 内容与已应用值一致（含启动对账一致）
        }
        match apply_config(&bytes, api_key_config, updates_enabled) {
            Ok(true) => {
                *applied = Some(bytes);
                return;
            }
            Ok(false) => return,
            Err(error) => {
                if attempt == max_retries {
                    warn!(
                        "[CONFIG_WATCHER] config reload failed after {} attempts (keeping previous config): {error:#}",
                        max_retries
                    );
                } else {
                    retry_delay *= 2;
                }
            }
        }
    }
}

/// 解析并提交（ArcSwap 原子换）。校验失败（缺段/空 key/解析错误）返回 Err，
/// 不写入 ArcSwap——运行配置保持旧值。返回 false 表示 watcher 已退出。
fn apply_config(
    bytes: &[u8],
    api_key_config: &Arc<ArcSwap<ApiKeyAuthConfig>>,
    updates_enabled: &Mutex<bool>,
) -> Result<bool> {
    let content = std::str::from_utf8(bytes).context("decode config file as utf-8")?;
    let new_config = parse_api_key_config(content).map_err(|error| {
        error!("[CONFIG_WATCHER] config rejected (keeping previous): {error:#}");
        error
    })?;
    if new_config.enabled && new_config.api_key.trim().is_empty() {
        error!("[CONFIG_WATCHER] API Key is empty");
        anyhow::bail!("API Key cannot be empty string");
    }

    let new_config = Arc::new(new_config);
    let old_config = {
        let enabled = updates_enabled.lock();
        if !*enabled {
            return Ok(false);
        }
        // No parsing, filesystem I/O or logging while holding this guard.
        api_key_config.swap(Arc::clone(&new_config))
    };
    let old_enabled = old_config.enabled;
    let key_changed = old_config.api_key != new_config.api_key;
    let new_enabled = new_config.enabled;

    if old_enabled != new_enabled {
        info!(
            "🔄 [CONFIG_WATCHER] API Key auth status updated: {} -> {}",
            old_enabled, new_enabled
        );
    }
    if key_changed {
        info!("[CONFIG_WATCHER] API Key updated");
    }
    if old_enabled == new_enabled && !key_changed {
        return Ok(true); // 配置未实际变化，不记录成功日志
    }
    info!("[CONFIG_WATCHER] Config update succeeded");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn config_text(enabled: bool, key: &str) -> String {
        format!("\napi_key_auth:\n  enabled: {enabled}\n  api_key: \"{key}\"\n")
    }

    fn write_file(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn shared_config(enabled: bool, key: &str) -> Arc<ArcSwap<ApiKeyAuthConfig>> {
        Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled,
            api_key: key.to_string(),
        }))
    }

    async fn wait_until(
        api_key_config: &Arc<ArcSwap<ApiKeyAuthConfig>>,
        expect_enabled: bool,
        expect_key: &str,
        budget: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + budget;
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
        write_file(&config_path, &config_text(false, "sk-test123"));
        let watcher = ConfigWatcher::with_poll_interval(
            config_path,
            shared_config(false, "sk-test123"),
            Duration::ZERO,
        );
        assert!(watcher.is_ok());
    }

    /// 半成品与非法配置必须拒绝，且不得推进已应用快照。直接等待真实
    /// 重载流程完成，避免仅因后台任务尚未运行而把旧值不变误判为通过。
    #[tokio::test(start_paused = true)]
    async fn invalid_configs_keep_auth_and_snapshot_then_allow_explicit_disable() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        let valid = config_text(true, "sk-valid");
        write_file(&config_path, &valid);
        let api_key_config = shared_config(true, "sk-valid");
        let updates_enabled = Mutex::new(true);
        let mut applied = None;
        reload_with_retry(
            &config_path,
            &mut applied,
            &api_key_config,
            &updates_enabled,
        )
        .await;
        for invalid in [
            "port: 8080\n",
            "api_key_auth: null\n",
            "api_key_auth:\n  api_key: sk-partial\n",
            "api_key_auth: [\n",
            "api_key_auth:\n  enabled: true\n  api_key: '  '\n",
        ] {
            write_file(&config_path, invalid);
            reload_with_retry(
                &config_path,
                &mut applied,
                &api_key_config,
                &updates_enabled,
            )
            .await;
            let current = api_key_config.load();
            assert!(current.enabled && current.api_key == "sk-valid");
            assert_eq!(applied.as_deref(), Some(valid.as_bytes()));
        }
        // 显式关闭才生效；其他配置段的类型变化不影响 API Key 热加载。
        let disabled = format!(
            "port: [unrelated-schema]\n{}",
            config_text(false, "sk-valid")
        );
        write_file(&config_path, &disabled);
        reload_with_retry(
            &config_path,
            &mut applied,
            &api_key_config,
            &updates_enabled,
        )
        .await;
        let current = api_key_config.load();
        assert!(!current.enabled && current.api_key == "sk-valid");
        assert_eq!(applied.as_deref(), Some(disabled.as_bytes()));
    }

    /// 启动对账：构造时文件内容与共享配置初值不同（启动读取与 watcher
    /// 创建之间被改过）——构造后立即对账应用，不把构造时刻内容当基线遗漏。
    #[tokio::test]
    async fn poll_only_reconciles_at_startup_and_tracks_changes() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_file(&config_path, &config_text(true, "sk-changed-before-watch"));
        let api_key_config = shared_config(false, "sk-old");
        let _watcher = ConfigWatcher::poll_only(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(100),
        )
        .unwrap();
        // 构造后立即对账（无需任何文件修改）
        wait_until(
            &api_key_config,
            true,
            "sk-changed-before-watch",
            Duration::from_secs(3),
        )
        .await;
        // 后续变更正常跟踪
        write_file(&config_path, &config_text(true, "sk-later"));
        wait_until(&api_key_config, true, "sk-later", Duration::from_secs(3)).await;
        // 文件暂时不可读（删除）：保持已应用配置；恢复后按内容处理
        fs::remove_file(&config_path).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        write_file(&config_path, &config_text(true, "sk-recovered"));
        wait_until(
            &api_key_config,
            true,
            "sk-recovered",
            Duration::from_secs(3),
        )
        .await;
    }

    /// 行为回归：原地写热加载生效（事件与轮询双通路形态，与生产一致）。
    #[tokio::test]
    async fn in_place_overwrite_triggers_hot_reload() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_file(&config_path, &config_text(false, "sk-old"));
        let api_key_config = shared_config(false, "sk-before-watch");
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(150),
        )
        .unwrap();
        wait_until(&api_key_config, false, "sk-old", Duration::from_secs(3)).await;
        write_file(&config_path, &config_text(true, "sk-new"));
        wait_until(&api_key_config, true, "sk-new", Duration::from_secs(5)).await;
    }

    /// 原子替换（write-to-temp + rename）触发目录监听路径——仅 Linux
    /// inotify 确定性覆盖（macOS fsevent 本机高负载下事件延迟不可复现判
    /// 定，行为级覆盖由轮询兜底测试承担）。
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rename_replacement_triggers_hot_reload() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_file(&config_path, &config_text(false, "sk-old"));
        let api_key_config = shared_config(false, "sk-before-watch");
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::ZERO,
        )
        .unwrap();
        // 先完成启动对账，后续更新必须由纯事件通路触发。
        wait_until(&api_key_config, false, "sk-old", Duration::from_secs(3)).await;
        let staging = temp_dir.path().join(".config.yml.new");
        write_file(&staging, &config_text(true, "sk-renamed"));
        fs::rename(&staging, &config_path).unwrap();
        wait_until(&api_key_config, true, "sk-renamed", Duration::from_secs(5)).await;
    }

    /// 连续两次修改快速连写：单一串行重载执行者保证最终收敛到最新值，
    /// 不出现旧值倒序覆盖。
    #[tokio::test]
    async fn rapid_double_write_converges_to_latest() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_file(&config_path, &config_text(false, "sk-old"));
        let api_key_config = shared_config(false, "sk-before-watch");
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(100),
        )
        .unwrap();
        wait_until(&api_key_config, false, "sk-old", Duration::from_secs(3)).await;
        write_file(&config_path, &config_text(true, "sk-a"));
        write_file(&config_path, &config_text(true, "sk-b"));
        wait_until(&api_key_config, true, "sk-b", Duration::from_secs(5)).await;
    }

    /// 生命周期：`ConfigWatcher` drop 后不再有任何任务更新配置。
    #[tokio::test]
    async fn drop_stops_reloading() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");
        write_file(&config_path, &config_text(false, "sk-old"));
        let api_key_config = shared_config(false, "sk-before-watch");
        let watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(100),
        )
        .unwrap();
        wait_until(&api_key_config, false, "sk-old", Duration::from_secs(3)).await;
        // 模拟已经开始的同步读取：保留发布入口，Drop 后返回读取结果。
        // 单靠 JoinHandle::abort 无法撤销同步段，因此必须拒绝迟到提交。
        let updates_enabled = Arc::clone(&watcher.updates_enabled);
        drop(watcher);
        assert!(
            !apply_config(
                config_text(true, "sk-in-flight").as_bytes(),
                &api_key_config,
                &updates_enabled,
            )
            .unwrap()
        );
        write_file(&config_path, &config_text(true, "sk-after-drop"));
        tokio::time::sleep(Duration::from_millis(600)).await;
        let current = api_key_config.load();
        assert!(
            !current.enabled && current.api_key == "sk-old",
            "dropped watcher must not reload: enabled={} key={}",
            current.enabled,
            current.api_key
        );
    }

    /// 文件和父目录在创建 watcher 时都不存在：注册失败仍保留轮询，
    /// 路径恢复后自动加载，不依赖再次启动 RCoder。
    #[tokio::test]
    async fn unavailable_native_watches_recover_when_config_appears() {
        let temp_dir = TempDir::new().unwrap();
        let parent = temp_dir.path().join("not-created-yet");
        let config_path = parent.join("config.yml");
        let api_key_config = shared_config(true, "sk-old");
        let _watcher = ConfigWatcher::with_poll_interval(
            config_path.clone(),
            api_key_config.clone(),
            Duration::from_millis(100),
        )
        .unwrap();
        // 先让启动对账经历一次失败，文件出现后必须靠后续重试恢复。
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(api_key_config.load().api_key, "sk-old");
        fs::create_dir(&parent).unwrap();
        write_file(&config_path, &config_text(true, "sk-recovered"));
        wait_until(
            &api_key_config,
            true,
            "sk-recovered",
            Duration::from_secs(3),
        )
        .await;
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

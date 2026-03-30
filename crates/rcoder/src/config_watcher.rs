//! 配置文件监控模块
//!
//! 本模块使用 `notify` crate 实现配置文件的实时监控和热更新。
//! 当 `config.yml` 文件被修改时,自动重新加载 API Key 配置,无需重启服务。
//!
//! # 功能特性
//!
//! - 实时监控配置文件变化(基于文件系统事件)
//! - 自动重载 API Key 配置
//! - 配置验证(防止空 API Key)
//! - 详细的变更日志记录
//! - 线程安全的配置更新(使用 RwLock)
//!
//! # 使用示例
//!
//! ```no_run
//! use std::sync::{Arc, RwLock};
//! use std::path::PathBuf;
//! # use crate::config::ApiKeyAuthConfig;
//! # use crate::config_watcher::ConfigWatcher;
//!
//! let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default()));
//! let config_path = PathBuf::from("config.yml");
//!
//! match ConfigWatcher::new(config_path, api_key_config) {
//!     Ok(watcher) => {
//! println!("config message alreadystarted");
//!         // watcher 必须保持存活,否则监控会停止
//!     }
//!     Err(e) => {
//! eprintln!("config message startedfailed: {}", e);
//!     }
//! }
//! ```

use anyhow::Result;
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

/// 配置文件监控器
///
/// 内部持有 `RecommendedWatcher` 以保持文件监控活跃。
/// 一旦 ConfigWatcher 被 drop,文件监控将停止。
///
/// # 注意
///
/// `config_path` 和 `api_key_config` 字段虽然未直接读取,
/// 但它们被 spawn 的异步任务闭包捕获,因此必须保留。
pub struct ConfigWatcher {
    /// 文件系统监控器(必须保持存活)
    _watcher: RecommendedWatcher,
    /// 配置文件路径(未直接访问,但被闭包捕获)
    #[allow(dead_code)]
    config_path: PathBuf,
    /// API Key 配置的共享引用(未直接访问,但被闭包捕获)
    #[allow(dead_code)]
    api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
}

impl ConfigWatcher {
    /// 创建新的配置监控器
    pub fn new(
        config_path: PathBuf,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    ) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel(100);

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        })?;

        watcher.watch(&config_path, RecursiveMode::NonRecursive)?;

 info!("📁 [CONFIG_WATCHER] starting message configfile: {:?}", config_path);

        // 克隆必要的数据以在 tokio 任务中使用
        let config_path_clone = config_path.clone();
        let api_key_config_clone = Arc::clone(&api_key_config);

        // 启动配置监控任务
        tokio::spawn(async move {
            loop {
                if let Some(event) = rx.recv().await {
                    // 只处理修改事件
                    if matches!(event.kind, EventKind::Modify(_)) {
                        // 添加短暂延迟，避免文件未完全写入
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        if let Err(e) = Self::reload_config(
                            &config_path_clone,
                            Arc::clone(&api_key_config_clone),
                        )
                        .await
                        {
 warn!(" [CONFIG_WATCHER] config message failed: {}", e);
                        }
                    }
                }
            }
        });

        Ok(Self {
            _watcher: watcher,
            config_path,
            api_key_config,
        })
    }

    /// 重新加载配置（使用 ArcSwap 无锁更新）
    async fn reload_config(
        config_path: &Path,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    ) -> Result<()> {
        match load_api_key_config_from_file(config_path) {
            Ok(new_config) => {
                // 验证配置有效性
                if new_config.enabled && new_config.api_key.trim().is_empty() {
 error!("[CONFIG_WATCHER] API Key message empty message ");
                    return Err(anyhow::anyhow!("API Key 不能为空字符串"));
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
                        "🔄 [CONFIG_WATCHER] API Key 鉴权状态已更新: {} -> {}",
                        old_enabled, new_enabled
                    );
                }

                if key_changed {
 info!("[CONFIG_WATCHER] API Key alreadyupdated");
                }

                if !old_enabled && !new_enabled && !key_changed {
                    // 配置未实际变化，不记录日志
                    return Ok(());
                }

 info!("[CONFIG_WATCHER] config message Update succeeded");
                Ok(())
            }
            Err(e) => {
 error!("[CONFIG_WATCHER] message configfilefailed: {}", e);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_config_watcher_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yml");

        // 创建测试配置文件
        fs::write(
            &config_path,
            r#"
api_key_auth:
  enabled: false
  api_key: "sk-test123"
"#,
        )
        .unwrap();

        let api_key_config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: false,
            api_key: "sk-test123".to_string(),
        }));

        let watcher = ConfigWatcher::new(config_path, api_key_config);
        assert!(watcher.is_ok());
    }
}

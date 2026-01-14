//! 测试用阻塞注入
//!
//! 用于模拟极端场景 (Agent 阻塞、超时等)

use std::sync::Arc;
use std::time::Duration;

/// 阻塞配置
#[derive(Debug, Clone)]
pub struct BlockingConfig {
    /// 是否阻塞 new_session
    pub block_new_session: bool,
    /// 是否阻塞 prompt
    pub block_prompt: bool,
    /// 阻塞持续时间
    pub block_duration: Duration,
}

impl Default for BlockingConfig {
    fn default() -> Self {
        Self {
            block_new_session: false,
            block_prompt: false,
            block_duration: Duration::from_secs(30),
        }
    }
}

/// 全局阻塞配置 (线程安全)
pub static BLOCKING_CONFIG: std::sync::LazyLock<Arc<std::sync::RwLock<BlockingConfig>>> =
    std::sync::LazyLock::new(|| Arc::new(std::sync::RwLock::new(BlockingConfig::default())));

/// 注入阻塞 (用于测试)
///
/// # Example
///
/// ```rust
/// use agent_runner::testing::blocking::{BlockingConfig, inject_blocking};
/// use std::time::Duration;
///
/// // 注入 30 秒阻塞
/// inject_blocking(BlockingConfig {
///     block_prompt: true,
///     block_duration: Duration::from_secs(30),
///     ..Default::default()
/// });
/// ```
pub fn inject_blocking(config: BlockingConfig) {
    tracing::warn!("🧪 [TEST] 阻塞配置已更新: {:?}", config);
    let mut global = BLOCKING_CONFIG.write().unwrap();
    *global = config;
}

/// 检查并执行阻塞 (在 agent_worker_with_heartbeat 中调用)
///
/// # Arguments
///
/// * `blocking_type` - 阻塞类型: "new_session" 或 "prompt"
///
/// # Example
///
/// ```rust
/// # use agent_runner::testing::blocking::maybe_block;
/// # async fn test() {
/// // 在 agent_worker_with_heartbeat 中调用
/// maybe_block("prompt").await;
/// # }
/// ```
pub async fn maybe_block(blocking_type: &str) {
    let config = BLOCKING_CONFIG.read().unwrap();

    let should_block = match blocking_type {
        "new_session" => config.block_new_session,
        "prompt" => config.block_prompt,
        _ => false,
    };

    if should_block {
        tracing::warn!(
            "🧪 [TEST] 注入阻塞: {}, 持续时间: {:?}",
            blocking_type,
            config.block_duration
        );
        tokio::time::sleep(config.block_duration).await;
    }
}

/// 重置阻塞配置 (用于测试清理)
pub fn reset_blocking() {
    inject_blocking(BlockingConfig::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocking_config_default() {
        let config = BlockingConfig::default();
        assert!(!config.block_new_session);
        assert!(!config.block_prompt);
        assert_eq!(config.block_duration, Duration::from_secs(30));
    }

    #[test]
    fn test_inject_and_reset_blocking() {
        // 注入配置
        inject_blocking(BlockingConfig {
            block_prompt: true,
            block_duration: Duration::from_secs(10),
            ..Default::default()
        });

        let config = BLOCKING_CONFIG.read().unwrap();
        assert!(config.block_prompt);
        assert_eq!(config.block_duration, Duration::from_secs(10));

        // 重置配置
        drop(config);
        reset_blocking();

        let config = BLOCKING_CONFIG.read().unwrap();
        assert!(!config.block_prompt);
        assert_eq!(config.block_duration, Duration::from_secs(30));
    }
}

//! API 密钥管理器
//!
//! 在内存中存储 API 密钥配置，支持通过服务名称快速查询。
//! 密钥配置通过 gRPC 从 rcoder 主服务传递到 agent_runner。
//!
//! 注意：当前 manager 由 binary 通过 gRPC 处理流程注入，lib 内部不直接调用
//! get/store/remove 等方法，故抑制 dead_code 警告。
//!
//! ## 使用示例
//!
//! ```rust
//! use agent_runner::api_key_manager::ApiKeyManager;
//! use shared_types::ModelProviderConfig;
//!
//! let manager = ApiKeyManager::new();
//!
//! // 存储 API 配置
//! let config = ModelProviderConfig {
//!     id: "anthropic-prov".to_string(),
//!     name: "anthropic".to_string(),
//!     api_key: "sk-ant-xxx".to_string(),
//!     base_url: "https://api.anthropic.com".to_string(),
//!     requires_openai_auth: false,
//!     default_model: "claude-3-5-sonnet-20241022".to_string(),
//!     api_protocol: Some("anthropic".to_string()),
//!     wire_api: None,
//! };
//! manager.store_config("anthropic", config);
//!
//! // 查询 API 密钥
//! if let Some(key) = manager.get_api_key("anthropic") {
//!     println!("API Key: {}", key);
//! }
//! ```

#![allow(dead_code)]

use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use std::sync::Arc;
use tracing::{debug, info};

/// API 密钥管理器
///
/// 使用 DashMap 实现并发安全的 HashMap，支持多线程安全访问。
/// 密钥配置存储在内存中，进程重启后清空。
///
/// 可以作为独立存储使用（通过 `new()`），也可以包装共享的 DashMap（通过 `from_shared()`）。
#[derive(Debug, Clone)]
pub struct ApiKeyManager {
    /// 服务名称 -> 密钥配置
    ///
    /// 键为服务标识符（如 UUID），值为完整的 ModelProviderConfig。
    /// 包装共享的 DashMap 引用（不拥有数据）或拥有独立 DashMap。
    shared: Arc<DashMap<String, ModelProviderConfig>>,
}

impl Default for ApiKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyManager {
    /// 创建新的 API 密钥管理器（拥有独立的 DashMap）
    ///
    /// # 示例
    ///
    /// ```rust
    /// use agent_runner::api_key_manager::ApiKeyManager;
    ///
    /// let manager = ApiKeyManager::new();
    /// ```
    pub fn new() -> Self {
        Self {
            shared: Arc::new(DashMap::new()),
        }
    }

    /// 从共享 DashMap 创建包装器
    ///
    /// 将 ApiKeyManager 改为包装共享的 DashMap，而不是拥有独立的数据。
    /// 这样多个组件可以共享同一个配置存储。
    ///
    /// # 参数
    ///
    /// * `shared` - 共享的 DashMap<String, ModelProviderConfig>
    ///
    /// # 示例
    ///
    /// ```rust
    /// use agent_runner::api_key_manager::ApiKeyManager;
    /// use dashmap::DashMap;
    /// use std::sync::Arc;
    ///
    /// let shared_map = Arc::new(DashMap::new());
    /// let manager = ApiKeyManager::from_shared(shared_map);
    /// ```
    pub fn from_shared(shared: Arc<DashMap<String, ModelProviderConfig>>) -> Self {
        Self { shared }
    }

    /// 存储 ModelProviderConfig 到内存
    ///
    /// 如果已存在相同服务名称的配置，将被覆盖。
    /// 使用 DashMap 的 insert 方法确保原子性。
    ///
    /// # 参数
    ///
    /// * `service_name` - 服务标识符（如 UUID）
    /// * `config` - 完整的模型提供商配置
    ///
    /// # 示例
    ///
    /// ```rust
    /// # use agent_runner::api_key_manager::ApiKeyManager;
    /// # use shared_types::ModelProviderConfig;
    /// let manager = ApiKeyManager::new();
    /// let config = ModelProviderConfig {
    ///     id: "anthropic-prov".to_string(),
    ///     name: "anthropic".to_string(),
    ///     api_key: "sk-ant-xxx".to_string(),
    ///     base_url: "https://api.anthropic.com".to_string(),
    ///     requires_openai_auth: false,
    ///     default_model: "claude-3-5-sonnet-20241022".to_string(),
    ///     api_protocol: Some("anthropic".to_string()),
    ///     wire_api: None,
    /// };
    /// manager.store_config("svc-uuid-123", config);
    /// ```
    pub fn store_config(&self, service_name: &str, config: ModelProviderConfig) {
        debug!(
            "🔑 [API_KEY_MANAGER] Storing config: service_name={}",
            service_name
        );
        self.shared.insert(service_name.to_string(), config);
    }

    /// 获取完整的 ModelProviderConfig
    ///
    /// 使用 DashMap 的 value() 方法避免克隆（返回引用）。
    ///
    /// # 参数
    ///
    /// * `service_name` - 服务标识符
    ///
    /// # 返回
    ///
    /// 如果找到配置则返回 `Some(ModelProviderConfig)`，否则返回 `None`。
    pub fn get(&self, service_name: &str) -> Option<ModelProviderConfig> {
        self.shared.get(service_name).map(|r| r.value().clone())
    }

    /// 获取 API 密钥
    ///
    /// # 参数
    ///
    /// * `service_name` - 服务标识符
    ///
    /// # 返回
    ///
    /// 如果找到配置则返回 `Some(api_key)`，否则返回 `None`。
    pub fn get_api_key(&self, service_name: &str) -> Option<String> {
        self.get(service_name).map(|c| c.api_key)
    }

    /// 获取 API Base URL
    ///
    /// # 参数
    ///
    /// * `service_name` - 服务标识符
    ///
    /// # 返回
    ///
    /// 如果找到配置则返回 `Some(base_url)`，否则返回 `None`。
    pub fn get_base_url(&self, service_name: &str) -> Option<String> {
        self.get(service_name).map(|c| c.base_url)
    }

    /// 移除指定服务的配置
    ///
    /// 使用 DashMap 的 remove 方法，这是原子性操作（entry API）。
    ///
    /// # 参数
    ///
    /// * `service_name` - 服务标识符
    ///
    /// # 返回
    ///
    /// 如果找到并移除了配置则返回 `Some(ModelProviderConfig)`，否则返回 `None`。
    pub fn remove(&self, service_name: &str) -> Option<ModelProviderConfig> {
        debug!(
            "🔑 [API_KEY_MANAGER] Removing config: service_name={}",
            service_name
        );
        self.shared.remove(service_name).map(|(_, v)| v)
    }

    /// 清空所有配置
    pub fn clear(&self) {
        info!("🔑 [API_KEY_MANAGER] Cleared all configs");
        self.shared.clear();
    }

    /// 获取当前存储的配置数量
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// 检查是否为空
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config(name: &str) -> ModelProviderConfig {
        ModelProviderConfig {
            id: format!("test_{}", name),
            name: name.to_string(),
            base_url: format!("https://api.{}.com", name),
            api_key: format!("sk-{}-123", name),
            requires_openai_auth: name == "openai",
            default_model: "test-model".to_string(),
            api_protocol: Some(name.to_string()),
            wire_api: None,
        }
    }

    #[test]
    fn test_store_and_get() {
        let manager = ApiKeyManager::new();
        let config = create_test_config("anthropic");

        manager.store_config("anthropic", config.clone());

        let retrieved = manager.get("anthropic");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().api_key, "sk-anthropic-123");
    }

    #[test]
    fn test_get_api_key() {
        let manager = ApiKeyManager::new();
        let config = create_test_config("openai");

        manager.store_config("openai", config);

        let api_key = manager.get_api_key("openai");
        assert_eq!(api_key, Some("sk-openai-123".to_string()));
    }

    #[test]
    fn test_get_base_url() {
        let manager = ApiKeyManager::new();
        let config = create_test_config("anthropic");

        manager.store_config("anthropic", config);

        let base_url = manager.get_base_url("anthropic");
        assert_eq!(base_url, Some("https://api.anthropic.com".to_string()));
    }

    #[test]
    fn test_nonexistent() {
        let manager = ApiKeyManager::new();

        assert!(manager.get("nonexistent").is_none());
        assert!(manager.get_api_key("nonexistent").is_none());
        assert!(manager.get_base_url("nonexistent").is_none());
    }

    #[test]
    fn test_remove() {
        let manager = ApiKeyManager::new();
        let config = create_test_config("anthropic");

        manager.store_config("anthropic", config);
        assert_eq!(manager.len(), 1);

        let removed = manager.remove("anthropic");
        assert!(removed.is_some());
        assert_eq!(manager.len(), 0);
        assert!(manager.get("anthropic").is_none());
    }

    #[test]
    fn test_clear() {
        let manager = ApiKeyManager::new();

        manager.store_config("anthropic", create_test_config("anthropic"));
        manager.store_config("openai", create_test_config("openai"));

        assert_eq!(manager.len(), 2);
        manager.clear();
        assert_eq!(manager.len(), 0);
    }

    #[test]
    fn test_overwrite() {
        let manager = ApiKeyManager::new();

        manager.store_config("anthropic", create_test_config("anthropic"));

        let mut new_config = create_test_config("anthropic");
        new_config.api_key = "sk-updated".to_string();

        manager.store_config("anthropic", new_config);

        let api_key = manager.get_api_key("anthropic");
        assert_eq!(api_key, Some("sk-updated".to_string()));
    }

    #[test]
    fn test_is_empty() {
        let manager = ApiKeyManager::new();
        assert!(manager.is_empty());

        manager.store_config("anthropic", create_test_config("anthropic"));
        assert!(!manager.is_empty());
    }
}

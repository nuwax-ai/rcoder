//! 多镜像配置结构
//!
//! 定义了支持多种服务类型的 Docker 镜像配置系统，包括全局默认配置、
//! 服务特定配置、选择策略和缓存机制。

use crate::service_config::ServiceImageConfig;
use crate::service_type::ServiceType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// 检查服务名称是否与 ServiceType 兼容
///
/// 支持旧的服务名称（如 "rcoder"）与新的 ServiceType（如 "web-agent-runner"）配对
fn is_compatible_service_key(service_key: &str, service_type: &ServiceType) -> bool {
    match service_type {
        ServiceType::WebAgentRunner => {
            // 兼容旧的服务名称 "rcoder"
            service_key == "rcoder"
        }
        ServiceType::ComputerAgentRunner => {
            // ComputerAgentRunner 没有旧名称
            false
        }
        ServiceType::UserApp | ServiceType::UserAppBuilder => {
            // UserApp / UserAppBuilder 镜像由调用方/image_selector 提供,
            // 不走多镜像配置选择,无旧名称兼容
            false
        }
    }
}

/// 多镜像配置结构
///
/// 支持多种服务类型的 Docker 镜像配置系统，提供灵活的镜像选择策略。
/// 注意：不包含 default_service_type，强制要求明确指定服务类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiImageConfig {
    /// 全局默认镜像配置
    pub global_defaults: GlobalImageDefaults,
    /// 各服务类型的镜像配置
    pub services: HashMap<String, ServiceImageConfig>,
    /// 镜像选择策略
    pub selection_strategy: ImageSelectionStrategy,
    /// 镜像缓存配置
    pub cache_config: ImageCacheConfig,
}

/// 全局默认镜像配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalImageDefaults {
    /// 通用镜像（优先级最高）
    pub image: Option<String>,
    /// 默认 ARM64 镜像
    pub arm64_image: Option<String>,
    /// 默认 AMD64 镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    pub default_image: Option<String>,
    /// 镜像仓库前缀
    pub registry_prefix: Option<String>,
}

/// 镜像选择策略
///
/// 当前只支持 ServiceOnly 策略，强制使用服务特定配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum ImageSelectionStrategy {
    /// 仅使用服务特定配置（强制明确指定服务类型）
    #[default]
    ServiceOnly,
}

/// 镜像缓存配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageCacheConfig {
    /// 是否启用镜像缓存
    pub enabled: bool,
    /// 缓存过期时间（秒）
    pub ttl_seconds: u64,
    /// 最大缓存条目数
    pub max_entries: usize,
}

/// 项目级镜像覆盖配置
///
/// 允许在项目级别覆盖镜像配置和环境变量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectImageOverrides {
    /// 项目特定的镜像覆盖
    pub images: HashMap<String, String>,
    /// 启用的服务类型列表
    pub enabled_services: Vec<String>,
    /// 项目特定的环境变量
    pub environment: HashMap<String, String>,
}

/// Config validation error
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config parse error: {0}")]
    ParseError(String),
    #[error("config validation error: {0}")]
    ValidationError(String),
    #[error("service type '{0}' not found")]
    ServiceNotFound(String),
    #[error("service type '{0}' is disabled")]
    ServiceDisabled(String),
    #[error("image config error: {0}")]
    ImageConfigError(String),
}

impl MultiImageConfig {
    /// 验证多镜像配置的有效性
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 验证全局默认配置
        if let Some(ref prefix) = self.global_defaults.registry_prefix
            && prefix.trim().is_empty()
        {
            return Err(ConfigError::ValidationError(
                "Image registry prefix cannot be empty".to_string(),
            ));
        }

        // 验证缓存配置
        if self.cache_config.enabled {
            if self.cache_config.ttl_seconds == 0 {
                return Err(ConfigError::ValidationError(
                    "Cache TTL must be greater than 0".to_string(),
                ));
            }

            if self.cache_config.max_entries == 0 {
                return Err(ConfigError::ValidationError(
                    "Maximum cache entries must be greater than 0".to_string(),
                ));
            }
        }

        // 验证服务配置
        for (service_key, service_config) in &self.services {
            // 验证服务名称一致性（兼容旧的服务名称）
            // 允许服务名称与 service_type 的字符串表示不完全匹配
            // 例如：服务名称 "rcoder" 可以与 service_type "web-agent-runner" 配对
            let expected_key = service_config.service_type.to_string();
            if service_key != &expected_key
                && !is_compatible_service_key(service_key, &service_config.service_type)
            {
                return Err(ConfigError::ValidationError(format!(
                    "Service key '{}' does not match service type '{}' (expected '{}')",
                    service_key, service_config.service_type, expected_key
                )));
            }

            // 验证服务配置
            match service_config.validate() {
                crate::service_config::ConfigValidationResult::Valid => {
                    // 配置有效
                }
                crate::service_config::ConfigValidationResult::Warning(warning) => {
                    tracing::warn!("Warning in '{}' config: {}", service_key, warning);
                }
                crate::service_config::ConfigValidationResult::Error(error) => {
                    return Err(ConfigError::ValidationError(format!(
                        "Service '{}' config error: {}",
                        service_key, error
                    )));
                }
            }
        }

        // 验证至少有一个启用的服务
        let enabled_services = self.list_enabled_services();
        if enabled_services.is_empty() {
            return Err(ConfigError::ValidationError(
                "At least one service type must be enabled".to_string(),
            ));
        }

        Ok(())
    }

    /// 获取指定服务类型的配置
    ///
    /// 支持通过新的服务名称（如 "web-agent-runner"）或旧的服务名称（如 "rcoder"）查找配置
    pub fn get_service_config(&self, service_type: &ServiceType) -> Option<&ServiceImageConfig> {
        // 1. 先尝试通过新的服务名称查找
        let service_key = service_type.to_string();
        if let Some(config) = self.services.get(&service_key) {
            return Some(config);
        }

        // 2. 如果找不到，尝试通过旧的服务名称查找
        match service_type {
            ServiceType::WebAgentRunner => {
                // 兼容旧的服务名称 "rcoder"
                self.services.get("rcoder")
            }
            ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 没有旧名称
                None
            }
            ServiceType::UserApp | ServiceType::UserAppBuilder => {
                // UserApp / UserAppBuilder 镜像由调用方/image_selector 提供,不走多镜像配置选择
                None
            }
        }
    }

    /// 获取指定服务类型的可变配置
    pub fn get_service_config_mut(
        &mut self,
        service_type: &ServiceType,
    ) -> Option<&mut ServiceImageConfig> {
        let service_key = service_type.to_string();
        self.services.get_mut(&service_key)
    }

    /// 添加或更新服务配置
    pub fn set_service_config(&mut self, service_type: ServiceType, config: ServiceImageConfig) {
        let service_key = service_type.to_string();
        self.services.insert(service_key, config);
    }

    /// 获取启用的服务类型列表
    pub fn list_enabled_services(&self) -> Vec<String> {
        self.services
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(service_key, _)| service_key.clone())
            .collect()
    }

    /// 获取所有支持的服务类型列表
    pub fn list_all_services(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// 检查服务是否已启用
    pub fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        self.get_service_config(service_type)
            .map(|config| config.enabled)
            .unwrap_or(false)
    }

    /// 启用或禁用服务
    pub fn set_service_enabled(
        &mut self,
        service_type: &ServiceType,
        enabled: bool,
    ) -> Result<(), ConfigError> {
        if let Some(config) = self.get_service_config_mut(service_type) {
            config.enabled = enabled;
            Ok(())
        } else {
            Err(ConfigError::ServiceNotFound(service_type.to_string()))
        }
    }

    /// 获取全局镜像前缀
    pub fn get_registry_prefix(&self) -> String {
        self.global_defaults
            .registry_prefix
            .clone()
            .unwrap_or_default()
    }

    /// 应用全局默认配置到服务配置
    pub fn apply_global_defaults(&mut self) {
        for (service_key, service_config) in self.services.iter_mut() {
            // 如果服务配置没有设置镜像，使用全局默认
            if service_config.image.is_none() && self.global_defaults.image.is_some() {
                service_config.image = self.global_defaults.image.clone();
            }

            if service_config.arm64_image.is_none() && self.global_defaults.arm64_image.is_some() {
                service_config.arm64_image = self.global_defaults.arm64_image.clone();
            }

            if service_config.amd64_image.is_none() && self.global_defaults.amd64_image.is_some() {
                service_config.amd64_image = self.global_defaults.amd64_image.clone();
            }

            if service_config.default_image.is_none()
                && self.global_defaults.default_image.is_some()
            {
                service_config.default_image = self.global_defaults.default_image.clone();
            }

            tracing::debug!("Using default config for '{}'", service_key);
        }
    }

    /// 获取配置摘要
    pub fn get_summary(&self) -> String {
        let enabled_services = self.list_enabled_services();
        format!(
            "Services: {}/{} enabled, Strategy: {:?}, Cache: {}",
            enabled_services.len(),
            self.services.len(),
            self.selection_strategy,
            if self.cache_config.enabled {
                "enabled"
            } else {
                "disabled"
            }
        )
    }
}

impl Default for MultiImageConfig {
    fn default() -> Self {
        let mut services = HashMap::new();

        // 添加默认的 RCoder 服务配置
        services.insert(
            ServiceType::WebAgentRunner.to_string(),
            crate::service_config::default_rcoder_service_config(),
        );

        // 添加默认的 ComputerAgentRunner 服务配置
        services.insert(
            ServiceType::ComputerAgentRunner.to_string(),
            crate::service_config::default_agent_runner_service_config(),
        );

        MultiImageConfig {
            global_defaults: GlobalImageDefaults {
                image: None,
                arm64_image: None,
                amd64_image: None,
                default_image: None,
                registry_prefix: None, // 从配置文件加载
            },
            services,
            selection_strategy: ImageSelectionStrategy::ServiceOnly,
            cache_config: ImageCacheConfig {
                enabled: true,
                ttl_seconds: 3600, // 1小时
                max_entries: 50,   // 适合双服务的缓存大小
            },
        }
    }
}

impl ProjectImageOverrides {
    /// 验证项目级配置
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 验证镜像覆盖配置
        for (service_type, image_name) in &self.images {
            if service_type.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "Service type name cannot be empty".to_string(),
                ));
            }

            if image_name.trim().is_empty() {
                return Err(ConfigError::ValidationError(format!(
                    "Image name for service type '{}' cannot be empty",
                    service_type
                )));
            }
        }

        // 验证启用的服务类型
        for service_type in &self.enabled_services {
            if service_type.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "Enabled service type name cannot be empty".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// 应用项目级配置到服务配置
    pub fn apply_to_service_config(
        &self,
        service_type: &ServiceType,
        config: &mut ServiceImageConfig,
    ) -> Result<(), ConfigError> {
        let service_key = service_type.to_string();

        // 应用镜像覆盖
        if let Some(override_image) = self.images.get(&service_key) {
            config.image = Some(override_image.clone());
            tracing::info!(
                "Applying project-level image override to service '{}': {}",
                service_key,
                override_image
            );
        }

        // 应用环境变量覆盖
        for (key, value) in &self.environment {
            config.environment.insert(key.clone(), value.clone());
        }

        Ok(())
    }

    /// 生成配置哈希键（用于缓存）
    ///
    /// 注意：使用 `DefaultHasher`，其输出**不保证跨 Rust 版本稳定**，故仅限
    /// 进程内缓存 key 使用，不可作为持久化 key 或跨进程比对依据。
    pub fn hash_key(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // 哈希镜像配置
        for (key, value) in &self.images {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        // 哈希启用的服务
        for service in &self.enabled_services {
            service.hash(&mut hasher);
        }

        // 哈希环境变量
        for (key, value) in &self.environment {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        format!("{:x}", hasher.finish())
    }

    /// 检查服务是否在项目级配置中启用
    pub fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        let service_key = service_type.to_string();
        self.enabled_services.contains(&service_key)
    }

    /// 获取配置摘要
    pub fn get_summary(&self) -> String {
        format!(
            "Images: {}, Enabled Services: {}, Environment Variables: {}",
            self.images.len(),
            self.enabled_services.len(),
            self.environment.len()
        )
    }
}

/// 创建默认的多镜像配置
pub fn create_default_multi_image_config() -> MultiImageConfig {
    MultiImageConfig::default()
}

/// 从传统配置创建多镜像配置
pub fn create_legacy_multi_image_config(
    image: Option<String>,
    arm64_image: Option<String>,
    amd64_image: Option<String>,
    default_image: Option<String>,
) -> MultiImageConfig {
    let global_defaults = GlobalImageDefaults {
        image,
        arm64_image,
        amd64_image,
        default_image,
        registry_prefix: None, // 从配置文件加载
    };

    // 如果设置了传统镜像配置，创建一个默认的 RCoder 服务配置
    let rcoder_config = if global_defaults.image.is_some()
        || global_defaults.arm64_image.is_some()
        || global_defaults.amd64_image.is_some()
        || global_defaults.default_image.is_some()
    {
        let mut config = crate::service_config::default_rcoder_service_config();
        config.image = global_defaults.image.clone();
        config.arm64_image = global_defaults.arm64_image.clone();
        config.amd64_image = global_defaults.amd64_image.clone();
        config.default_image = global_defaults.default_image.clone();
        config
    } else {
        crate::service_config::default_rcoder_service_config()
    };

    let mut services = HashMap::new();
    services.insert(ServiceType::WebAgentRunner.to_string(), rcoder_config);

    MultiImageConfig {
        global_defaults,
        services,
        selection_strategy: ImageSelectionStrategy::ServiceOnly,
        cache_config: ImageCacheConfig {
            enabled: true,
            ttl_seconds: 3600,
            max_entries: 50,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_config::default_rcoder_service_config;

    #[test]
    fn test_default_multi_image_config() {
        let config = MultiImageConfig::default();

        // 验证默认配置
        assert!(matches!(
            config.selection_strategy,
            ImageSelectionStrategy::ServiceOnly
        ));
        assert_eq!(config.services.len(), 2); // rcoder + computer-agent-runner
        assert!(config.is_service_enabled(&ServiceType::WebAgentRunner));
        assert!(config.is_service_enabled(&ServiceType::ComputerAgentRunner)); // 默认启用

        // 验证配置摘要
        let summary = config.get_summary();
        assert!(summary.contains("2/2")); // 2个启用，总共2个
    }

    #[test]
    fn test_config_validation() {
        let mut config = MultiImageConfig::default();

        // 为测试设置镜像配置
        for (_, service_config) in config.services.iter_mut() {
            service_config.arm64_image = Some("test-image:arm64".to_string());
            service_config.amd64_image = Some("test-image:amd64".to_string());
        }

        // 有效配置应该通过验证
        assert!(config.validate().is_ok());

        // 测试无效配置
        let mut invalid_config = config.clone();
        invalid_config.services.clear(); // 清空所有服务
        assert!(invalid_config.validate().is_err());
    }

    #[test]
    fn test_service_management() {
        let mut config = MultiImageConfig::default();

        // 测试服务启用/禁用
        assert!(
            config
                .set_service_enabled(&ServiceType::WebAgentRunner, false)
                .is_ok()
        );
        assert!(!config.is_service_enabled(&ServiceType::WebAgentRunner));

        assert!(
            config
                .set_service_enabled(&ServiceType::WebAgentRunner, true)
                .is_ok()
        );
        assert!(config.is_service_enabled(&ServiceType::WebAgentRunner));

        // 测试不存在的服务
        assert!(
            config
                .set_service_enabled(&ServiceType::ComputerAgentRunner, true)
                .is_ok()
        ); // 存在
    }

    #[test]
    fn test_legacy_config_creation() {
        let config = create_legacy_multi_image_config(
            Some("custom-registry.com/rcoder:latest".to_string()),
            None,
            None,
            None,
        );

        // 验证传统镜像配置被正确应用
        let rcoder_config = config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            rcoder_config.image,
            Some("custom-registry.com/rcoder:latest".to_string())
        );

        // 验证只有 WebAgentRunner 服务
        assert_eq!(config.services.len(), 1);
        assert!(config.services.contains_key("web-agent-runner"));
    }

    #[test]
    fn test_project_overrides() {
        let mut overrides = ProjectImageOverrides {
            images: HashMap::new(),
            enabled_services: vec!["web-agent-runner".to_string()],
            environment: HashMap::new(),
        };

        overrides.images.insert(
            "web-agent-runner".to_string(),
            "custom-web-agent-runner:latest".to_string(),
        );
        overrides
            .environment
            .insert("DEBUG".to_string(), "true".to_string());

        assert!(overrides.validate().is_ok());

        // 测试应用配置
        let mut service_config = default_rcoder_service_config();
        overrides
            .apply_to_service_config(&ServiceType::WebAgentRunner, &mut service_config)
            .unwrap();

        assert_eq!(
            service_config.image,
            Some("custom-web-agent-runner:latest".to_string())
        );
        assert!(service_config.environment.contains_key("DEBUG"));
    }

    #[test]
    fn test_apply_global_defaults() {
        let mut config = MultiImageConfig::default();

        // 设置全局默认配置
        config.global_defaults.image = Some("global-default:latest".to_string());

        // 应用全局默认配置
        config.apply_global_defaults();

        // 验证配置被应用
        for service_config in config.services.values() {
            assert_eq!(
                service_config.image,
                Some("global-default:latest".to_string())
            );
        }
    }

    #[test]
    fn test_registry_prefix() {
        let mut config = MultiImageConfig::default();

        // 测试默认前缀（空字符串）
        assert_eq!(config.get_registry_prefix(), "");

        // 测试自定义前缀
        config.global_defaults.registry_prefix = Some("my-registry.com".to_string());
        assert_eq!(config.get_registry_prefix(), "my-registry.com");
    }

    #[test]
    fn test_config_file_loading() {
        // 测试从 JSON 配置加载配置
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev"
  },
  "services": {
    "web-agent-runner": {
      "service_type": "web-agent-runner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "image_tag_prefix": "dev-master-rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "computer-agent-runner",
      "image": "dev-rcoder-agent-runner:latest",
      "arm64_image": "dev-rcoder-agent-runner:latest",
      "amd64_image": "dev-rcoder-agent-runner:latest",
      "default_image": "dev-rcoder-agent-runner:latest",
      "image_tag_prefix": "dev-rcoder-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证 web-agent-runner 配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            web_config.image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest".to_string())
        );
        assert!(web_config.enabled);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .unwrap();
        assert_eq!(
            computer_config.image,
            Some("dev-rcoder-agent-runner:latest".to_string())
        );
        assert!(computer_config.enabled);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }

    #[test]
    fn test_config_with_legacy_service_key() {
        // 测试服务名称是 "rcoder"，但 service_type 字段是 "web-agent-runner" 的配置
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev"
  },
  "services": {
    "rcoder": {
      "service_type": "web-agent-runner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "image_tag_prefix": "dev-master-rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "computer-agent-runner",
      "image": "dev-rcoder-agent-runner:latest",
      "arm64_image": "dev-rcoder-agent-runner:latest",
      "amd64_image": "dev-rcoder-agent-runner:latest",
      "default_image": "dev-rcoder-agent-runner:latest",
      "image_tag_prefix": "dev-rcoder-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证通过新的服务名称可以找到配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            web_config.image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest".to_string())
        );
        assert!(web_config.enabled);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .unwrap();
        assert_eq!(
            computer_config.image,
            Some("dev-rcoder-agent-runner:latest".to_string())
        );
        assert!(computer_config.enabled);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }

    #[test]
    fn test_config_from_local_config_file() {
        // 测试从本地配置文件 docker/config.yml 加载配置
        // 这是本地开发测试使用的配置文件
        // 使用相对路径读取项目根目录下的 docker/config.yml
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = std::path::Path::new(manifest_dir)
            .ancestors()
            .find(|p| p.join("docker/config.yml").exists())
            .expect("Could not find project root with docker/config.yml")
            .join("docker/config.yml");

        let config_content = std::fs::read_to_string(&config_path)
            .unwrap_or_else(|e| panic!("Failed to read config file at {:?}: {}", config_path, e));

        // 解析 YAML 配置
        let config: serde_yaml::Value = serde_yaml::from_str(&config_content)
            .unwrap_or_else(|e| panic!("Failed to parse YAML config: {}", e));

        // 提取 multi_image_config 部分
        let multi_image_config = config
            .get("docker_config")
            .and_then(|dc| dc.get("multi_image_config"))
            .expect("multi_image_config not found in config file");

        // 转换为 MultiImageConfig
        let multi_config: MultiImageConfig = serde_yaml::from_value(multi_image_config.clone())
            .unwrap_or_else(|e| panic!("Failed to parse multi_image_config: {}", e));

        // 验证服务数量(web-agent-runner + computer-agent-runner + user-app-builder)
        assert_eq!(multi_config.services.len(), 3);

        // 验证 web-agent-runner 配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .expect("web-agent-runner config not found");
        assert!(web_config.image.is_some());
        assert!(web_config.enabled);
        assert_eq!(web_config.service_type, ServiceType::WebAgentRunner);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .expect("computer-agent-runner config not found");
        assert!(computer_config.image.is_some());
        assert!(computer_config.enabled);
        assert_eq!(
            computer_config.service_type,
            ServiceType::ComputerAgentRunner
        );

        // 验证 user-app-builder 配置(路 B)
        let builder_config = multi_config
            .get_service_config(&ServiceType::UserAppBuilder)
            .expect("user-app-builder config not found");
        assert!(builder_config.image.is_some());
        assert!(builder_config.enabled);
        assert_eq!(builder_config.service_type, ServiceType::UserAppBuilder);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());

        // 验证通过 ServiceType 枚举可以找到配置
        assert!(
            multi_config
                .get_service_config(&ServiceType::WebAgentRunner)
                .is_some()
        );
        assert!(
            multi_config
                .get_service_config(&ServiceType::ComputerAgentRunner)
                .is_some()
        );

        // 输出配置摘要
        println!(
            "✅ Local config loaded: {} services, registry_prefix={:?}",
            multi_config.services.len(),
            multi_config.global_defaults.registry_prefix
        );
        for (key, svc) in &multi_config.services {
            println!(
                "  - {}: service_type={}, image={:?}, enabled={}",
                key, svc.service_type, svc.image, svc.enabled
            );
        }
    }

    #[test]
    fn test_config_with_rcoder_key_and_web_agent_runner_type() {
        // 测试服务名称是 "rcoder"，但 service_type 字段是 "WebAgentRunner" 的配置
        // 这是测试环境使用的配置格式
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test"
  },
  "services": {
    "rcoder": {
      "service_type": "WebAgentRunner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "image_tag_prefix": "rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "ComputerAgentRunner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "image_tag_prefix": "computer-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证通过 ServiceType 枚举可以找到配置
        let web_config = multi_config.get_service_config(&ServiceType::WebAgentRunner);
        assert!(
            web_config.is_some(),
            "Should find config for WebAgentRunner"
        );
        assert_eq!(
            web_config.unwrap().image,
            Some(
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest"
                    .to_string()
            )
        );

        let computer_config = multi_config.get_service_config(&ServiceType::ComputerAgentRunner);
        assert!(
            computer_config.is_some(),
            "Should find config for ComputerAgentRunner"
        );
        assert_eq!(
            computer_config.unwrap().image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest".to_string())
        );

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }
}

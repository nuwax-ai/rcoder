//! 多镜像配置结构
//!
//! 定义了支持多种服务类型的 Docker 镜像配置系统，包括全局默认配置、
//! 服务特定配置、选择策略和缓存机制。

use crate::service_config::ServiceImageConfig;
use crate::service_type::ServiceType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageSelectionStrategy {
    /// 仅使用服务特定配置（强制明确指定服务类型）
    ServiceOnly,
}

impl Default for ImageSelectionStrategy {
    fn default() -> Self {
        ImageSelectionStrategy::ServiceOnly
    }
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

/// 配置验证错误
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("配置解析错误: {0}")]
    ParseError(String),
    #[error("配置验证错误: {0}")]
    ValidationError(String),
    #[error("服务类型 '{0}' 未找到")]
    ServiceNotFound(String),
    #[error("服务类型 '{0}' 未启用")]
    ServiceDisabled(String),
    #[error("镜像配置错误: {0}")]
    ImageConfigError(String),
}

impl MultiImageConfig {
    /// 验证多镜像配置的有效性
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 验证全局默认配置
        if let Some(ref prefix) = self.global_defaults.registry_prefix {
            if prefix.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "镜像仓库前缀不能为空".to_string(),
                ));
            }
        }

        // 验证缓存配置
        if self.cache_config.enabled {
            if self.cache_config.ttl_seconds == 0 {
                return Err(ConfigError::ValidationError(
                    "缓存过期时间必须大于0".to_string(),
                ));
            }

            if self.cache_config.max_entries == 0 {
                return Err(ConfigError::ValidationError(
                    "最大缓存条目数必须大于0".to_string(),
                ));
            }
        }

        // 验证服务配置
        for (service_key, service_config) in &self.services {
            // 验证服务名称一致性
            if service_config.service_type.to_string() != *service_key {
                return Err(ConfigError::ValidationError(format!(
                    "服务键 '{}'与服务类型 '{}' 不匹配",
                    service_key,
                    service_config.service_type
                )));
            }

            // 验证服务配置
            match service_config.validate() {
                crate::service_config::ConfigValidationResult::Valid => {
                    // 配置有效
                }
                crate::service_config::ConfigValidationResult::Warning(warning) => {
                    tracing::warn!("服务 '{}' 配置警告: {}", service_key, warning);
                }
                crate::service_config::ConfigValidationResult::Error(error) => {
                    return Err(ConfigError::ValidationError(format!(
                        "服务 '{}' 配置错误: {}",
                        service_key, error
                    )));
                }
            }
        }

        // 验证至少有一个启用的服务
        let enabled_services = self.list_enabled_services();
        if enabled_services.is_empty() {
            return Err(ConfigError::ValidationError(
                "至少需要启用一个服务类型".to_string(),
            ));
        }

        Ok(())
    }

    /// 获取指定服务类型的配置
    pub fn get_service_config(&self, service_type: &ServiceType) -> Option<&ServiceImageConfig> {
        let service_key = service_type.to_string();
        self.services.get(&service_key)
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
            Err(ConfigError::ServiceNotFound(
                service_type.to_string(),
            ))
        }
    }

    /// 获取全局镜像前缀
    pub fn get_registry_prefix(&self) -> String {
        self.global_defaults
            .registry_prefix
            .clone()
            .unwrap_or_else(|| "registry.yichamao.com".to_string())
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

            tracing::debug!("应用全局默认配置到服务 '{}'", service_key);
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
            "rcoder".to_string(),
            crate::service_config::default_rcoder_service_config(),
        );

        // 添加默认的 ComputerAgentRunner 服务配置
        services.insert(
            "computer-agent-runner".to_string(),
            crate::service_config::default_agent_runner_service_config(),
        );

        MultiImageConfig {
            global_defaults: GlobalImageDefaults {
                image: None,
                arm64_image: None,
                amd64_image: None,
                default_image: None,
                registry_prefix: Some("registry.yichamao.com".to_string()),
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
                    "服务类型名称不能为空".to_string(),
                ));
            }

            if image_name.trim().is_empty() {
                return Err(ConfigError::ValidationError(format!(
                    "服务类型 '{}' 的镜像名称不能为空",
                    service_type
                )));
            }
        }

        // 验证启用的服务类型
        for service_type in &self.enabled_services {
            if service_type.trim().is_empty() {
                return Err(ConfigError::ValidationError(
                    "启用的服务类型名称不能为空".to_string(),
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
                "应用项目级镜像覆盖到服务 '{}': {}",
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
        registry_prefix: Some("registry.yichamao.com".to_string()),
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
    services.insert("rcoder".to_string(), rcoder_config);

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
        assert!(config.is_service_enabled(&ServiceType::RCoder));
        assert!(!config.is_service_enabled(&ServiceType::ComputerAgentRunner)); // 默认禁用

        // 验证配置摘要
        let summary = config.get_summary();
        assert!(summary.contains("1/2")); // 1个启用，总共2个
    }

    #[test]
    fn test_config_validation() {
        let config = MultiImageConfig::default();

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
                .set_service_enabled(&ServiceType::RCoder, false)
                .is_ok()
        );
        assert!(!config.is_service_enabled(&ServiceType::RCoder));

        assert!(
            config
                .set_service_enabled(&ServiceType::RCoder, true)
                .is_ok()
        );
        assert!(config.is_service_enabled(&ServiceType::RCoder));

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
        let rcoder_config = config.get_service_config(&ServiceType::RCoder).unwrap();
        assert_eq!(
            rcoder_config.image,
            Some("custom-registry.com/rcoder:latest".to_string())
        );

        // 验证只有 RCoder 服务
        assert_eq!(config.services.len(), 1);
        assert!(config.services.contains_key("rcoder"));
    }

    #[test]
    fn test_project_overrides() {
        let mut overrides = ProjectImageOverrides {
            images: HashMap::new(),
            enabled_services: vec!["rcoder".to_string()],
            environment: HashMap::new(),
        };

        overrides
            .images
            .insert("rcoder".to_string(), "custom-rcoder:latest".to_string());
        overrides
            .environment
            .insert("DEBUG".to_string(), "true".to_string());

        assert!(overrides.validate().is_ok());

        // 测试应用配置
        let mut service_config = default_rcoder_service_config();
        overrides
            .apply_to_service_config(&ServiceType::RCoder, &mut service_config)
            .unwrap();

        assert_eq!(
            service_config.image,
            Some("custom-rcoder:latest".to_string())
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
        for (_, service_config) in &config.services {
            assert_eq!(
                service_config.image,
                Some("global-default:latest".to_string())
            );
        }
    }

    #[test]
    fn test_registry_prefix() {
        let mut config = MultiImageConfig::default();

        // 测试默认前缀
        assert_eq!(config.get_registry_prefix(), "registry.yichamao.com");

        // 测试自定义前缀
        config.global_defaults.registry_prefix = Some("my-registry.com".to_string());
        assert_eq!(config.get_registry_prefix(), "my-registry.com");
    }
}

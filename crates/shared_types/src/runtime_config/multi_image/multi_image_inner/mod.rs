//! 多镜像配置结构
//!
//! 定义了支持多种服务类型的 Docker 镜像配置系统，包括全局默认配置、
//! 服务特定配置、选择策略和缓存机制。

use crate::runtime_config::service::ConfigValidationResult;
use crate::{ServiceImageConfig, ServiceType};
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
        ServiceType::Userapp | ServiceType::UserappBuilder => {
            // Userapp / UserappBuilder 镜像由调用方/image_selector 提供,
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

/// 镜像缓存默认过期时间（秒，1 小时）——各 crate 的 ttl 默认值单一来源
pub const IMAGE_CACHE_DEFAULT_TTL_SECS: u64 = 3600;

/// 镜像缓存默认最大条目数
pub const IMAGE_CACHE_DEFAULT_MAX_ENTRIES: usize = 50;

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

impl Default for ImageCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_seconds: IMAGE_CACHE_DEFAULT_TTL_SECS,
            max_entries: IMAGE_CACHE_DEFAULT_MAX_ENTRIES,
        }
    }
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
    ImageError(String),
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
                ConfigValidationResult::Valid => {
                    // 配置有效
                }
                ConfigValidationResult::Warning(warning) => {
                    tracing::warn!("Warning in '{}' config: {}", service_key, warning);
                }
                ConfigValidationResult::Error(error) => {
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
            ServiceType::Userapp | ServiceType::UserappBuilder => {
                // Userapp / UserappBuilder 镜像由调用方/image_selector 提供,不走多镜像配置选择
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
            crate::default_rcoder_service_config(),
        );

        // 添加默认的 ComputerAgentRunner 服务配置
        services.insert(
            ServiceType::ComputerAgentRunner.to_string(),
            crate::default_agent_runner_service_config(),
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
            cache_config: ImageCacheConfig::default(), // 适合双服务的默认缓存参数
        }
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
        let mut config = crate::default_rcoder_service_config();
        config.image = global_defaults.image.clone();
        config.arm64_image = global_defaults.arm64_image.clone();
        config.amd64_image = global_defaults.amd64_image.clone();
        config.default_image = global_defaults.default_image.clone();
        config
    } else {
        crate::default_rcoder_service_config()
    };

    let mut services = HashMap::new();
    services.insert(ServiceType::WebAgentRunner.to_string(), rcoder_config);

    MultiImageConfig {
        global_defaults,
        services,
        selection_strategy: ImageSelectionStrategy::ServiceOnly,
        cache_config: ImageCacheConfig::default(),
    }
}


#[cfg(test)]
mod tests;

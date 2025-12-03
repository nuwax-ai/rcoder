//! 服务类型定义
//!
//! 定义 RCoder 系统支持的服务类型，目前包括 RCoder 和 AgentRunner 两种类型。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// 服务类型枚举
///
/// 定义了 RCoder 系统支持的服务类型，每个服务类型对应不同的 Docker 镜像和运行环境。
/// 注意：不实现 Default trait，强制要求明确指定服务类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
pub enum ServiceType {
    /// 标准 RCoder 服务 (当前使用)
    /// 提供完整的 AI 开发功能，包括项目管理、代码生成、文件操作等
    RCoder,
    /// Agent Runner 服务 (新功能，后续开发使用)
    /// 专注于代理运行和执行，提供轻量级的代理执行环境
    AgentRunner,
}

impl ServiceType {
    /// 获取服务类型的字符串表示
    pub fn as_str(&self) -> &str {
        match self {
            ServiceType::RCoder => "rcoder",
            ServiceType::AgentRunner => "agent-runner",
        }
    }

    /// 从字符串解析服务类型
    ///
    /// 如果传入的服务类型无效，会记录警告并默认返回 RCoder 服务
    pub fn from_str(s: &str) -> Self {
        match s {
            "rcoder" => ServiceType::RCoder,
            "agent-runner" => ServiceType::AgentRunner,
            _ => {
                tracing::warn!("未知的服务类型 '{}'，使用默认的 RCoder 服务", s);
                ServiceType::RCoder
            }
        }
    }

    /// 获取服务类型的描述
    pub fn description(&self) -> &str {
        match self {
            ServiceType::RCoder => "标准 RCoder 服务，提供完整的 AI 开发功能",
            ServiceType::AgentRunner => "Agent Runner 服务，专注于代理运行和执行",
        }
    }

    /// 检查服务是否在给定的多镜像配置中启用
    pub fn is_enabled(&self, config: &crate::MultiImageConfig) -> bool {
        let service_key = self.as_str();
        if let Some(service_config) = config.services.get(service_key) {
            service_config.enabled
        } else {
            tracing::warn!("服务类型 '{}' 未在配置中找到", service_key);
            false
        }
    }
}

/// 服务类型验证错误
#[derive(Debug, Error)]
pub enum ServiceTypeError {
    #[error("服务类型不能为空")]
    EmptyServiceType,
    #[error("不支持的服务类型 '{0}'，请使用 'rcoder' 或 'agent-runner'")]
    InvalidServiceType(String),
    #[error("服务类型 '{0}' 已禁用")]
    ServiceDisabled(String),
}

/// 验证服务类型是否有效
pub fn validate_service_type(service_type: &str) -> Result<ServiceType, ServiceTypeError> {
    if service_type.trim().is_empty() {
        return Err(ServiceTypeError::EmptyServiceType);
    }

    match service_type {
        "rcoder" => Ok(ServiceType::RCoder),
        "agent-runner" => Ok(ServiceType::AgentRunner),
        _ => Err(ServiceTypeError::InvalidServiceType(
            service_type.to_string(),
        )),
    }
}

/// 验证服务类型是否已启用
pub fn validate_service_enabled(
    service_type: &ServiceType,
    config: &crate::MultiImageConfig,
) -> Result<(), ServiceTypeError> {
    if !service_type.is_enabled(config) {
        return Err(ServiceTypeError::ServiceDisabled(
            service_type.as_str().to_string(),
        ));
    }
    Ok(())
}

/// 获取所有支持的服务类型
pub fn get_supported_service_types() -> Vec<String> {
    vec!["rcoder".to_string(), "agent-runner".to_string()]
}

/// 获取启用的服务类型列表
pub fn get_enabled_service_types(config: &crate::MultiImageConfig) -> Vec<String> {
    let supported = get_supported_service_types();
    supported
        .into_iter()
        .filter(|service_type| {
            let service = ServiceType::from_str(service_type);
            service.is_enabled(config)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MultiImageConfig;
    use std::collections::HashMap;

    fn create_test_config() -> MultiImageConfig {
        use crate::multi_image_config::{
            GlobalImageDefaults, ImageCacheConfig, ImageSelectionStrategy,
        };
        use crate::service_config::{ServiceImageConfig, ServiceResourceLimits};

        let mut services = HashMap::new();

        services.insert(
            "rcoder".to_string(),
            ServiceImageConfig {
                service_type: ServiceType::RCoder,
                image: None,
                arm64_image: Some("registry.yichamao.com/rcoder:arm64".to_string()),
                amd64_image: Some("registry.yichamao.com/rcoder:amd64".to_string()),
                default_image: None,
                image_tag_prefix: Some("rcoder".to_string()),
                enabled: true,
                environment: HashMap::new(),
                mounts: vec![],
                command: vec![],
                entrypoint: None,
                resource_limits: ServiceResourceLimits {
                    memory_limit: None,
                    cpu_limit: None,
                    swap_limit: None,
                    disk_limit: None,
                    process_limit: None,
                },
                work_dir: "/app".to_string(),
                network_mode: "bridge".to_string(),
                container_path_template: "/app/project_workspace/{project_id}".to_string(),
            },
        );

        services.insert(
            "agent-runner".to_string(),
            ServiceImageConfig {
                service_type: ServiceType::AgentRunner,
                image: None,
                arm64_image: Some("registry.yichamao.com/agent-runner:arm64".to_string()),
                amd64_image: Some("registry.yichamao.com/agent-runner:amd64".to_string()),
                default_image: None,
                image_tag_prefix: Some("agent-runner".to_string()),
                enabled: false, // 默认禁用
                environment: HashMap::new(),
                mounts: vec![],
                command: vec![],
                entrypoint: None,
                resource_limits: ServiceResourceLimits {
                    memory_limit: None,
                    cpu_limit: None,
                    swap_limit: None,
                    disk_limit: None,
                    process_limit: None,
                },
                work_dir: "/app".to_string(),
                network_mode: "bridge".to_string(),
                container_path_template: "/app/project_workspace/{project_id}".to_string(),
            },
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
                ttl_seconds: 3600,
                max_entries: 50,
            },
        }
    }

    #[test]
    fn test_service_type_basic() {
        assert_eq!(ServiceType::RCoder.as_str(), "rcoder");
        assert_eq!(ServiceType::AgentRunner.as_str(), "agent-runner");

        assert!(ServiceType::RCoder.description().contains("完整"));
        assert!(ServiceType::AgentRunner.description().contains("执行"));
    }

    #[test]
    fn test_service_type_from_str() {
        assert_eq!(ServiceType::from_str("rcoder"), ServiceType::RCoder);
        assert_eq!(
            ServiceType::from_str("agent-runner"),
            ServiceType::AgentRunner
        );

        // 未知类型应该默认返回 RCoder
        assert_eq!(ServiceType::from_str("unknown"), ServiceType::RCoder);
    }

    #[test]
    fn test_validate_service_type() {
        assert!(validate_service_type("rcoder").is_ok());
        assert!(validate_service_type("agent-runner").is_ok());

        assert!(validate_service_type("").is_err());
        assert!(validate_service_type("   ").is_err());
        assert!(validate_service_type("unknown").is_err());
    }

    #[test]
    fn test_service_type_enabled() {
        let config = create_test_config();

        // RCoder 应该启用
        assert!(ServiceType::RCoder.is_enabled(&config));

        // AgentRunner 应该禁用
        assert!(!ServiceType::AgentRunner.is_enabled(&config));
    }

    #[test]
    fn test_validate_service_enabled() {
        let config = create_test_config();

        // RCoder 应该通过验证
        assert!(validate_service_enabled(&ServiceType::RCoder, &config).is_ok());

        // AgentRunner 应该失败
        assert!(validate_service_enabled(&ServiceType::AgentRunner, &config).is_err());
    }

    #[test]
    fn test_get_supported_service_types() {
        let types = get_supported_service_types();
        assert_eq!(types.len(), 2);
        assert!(types.contains(&"rcoder".to_string()));
        assert!(types.contains(&"agent-runner".to_string()));
    }

    #[test]
    fn test_get_enabled_service_types() {
        let config = create_test_config();
        let enabled = get_enabled_service_types(&config);

        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains(&"rcoder".to_string()));
        assert!(!enabled.contains(&"agent-runner".to_string()));
    }

    #[test]
    fn test_service_type_serialization() {
        let service = ServiceType::RCoder;
        let serialized = serde_json::to_string(&service).unwrap();
        let deserialized: ServiceType = serde_json::from_str(&serialized).unwrap();

        assert_eq!(service, deserialized);
    }

    #[test]
    fn test_service_type_hash() {
        use std::hash::{Hash, Hasher};

        let mut hasher1 = std::collections::hash_map::DefaultHasher::new();
        let mut hasher2 = std::collections::hash_map::DefaultHasher::new();

        ServiceType::RCoder.hash(&mut hasher1);
        ServiceType::AgentRunner.hash(&mut hasher2);

        assert_ne!(hasher1.finish(), hasher2.finish());
    }
}

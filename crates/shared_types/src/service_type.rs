//! 服务类型定义
//!
//! 定义 RCoder 系统支持的服务类型，目前包括 RCoder 和 ComputerAgentRunner 两种类型。

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
    /// Computer Agent Runner 服务 (新功能，后续开发使用)
    /// 专注于代理运行和执行，提供轻量级的代理执行环境
    ComputerAgentRunner,
}

impl std::fmt::Display for ServiceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceType::RCoder => write!(f, "rcoder"),
            ServiceType::ComputerAgentRunner => write!(f, "computer-agent-runner"),
        }
    }
}

impl std::str::FromStr for ServiceType {
    type Err = ServiceTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 空字符串检查
        if s.trim().is_empty() {
            return Err(ServiceTypeError::EmptyServiceType);
        }

        // 精确匹配
        match s {
            "rcoder" => Ok(ServiceType::RCoder),
            "computer-agent-runner" => Ok(ServiceType::ComputerAgentRunner),
            _ => Err(ServiceTypeError::InvalidServiceType(s.to_string())),
        }
    }
}

impl ServiceType {
    /// 获取服务类型的描述
    pub fn description(&self) -> &str {
        match self {
            ServiceType::RCoder => "标准 RCoder 服务，提供完整的 AI 开发功能",
            ServiceType::ComputerAgentRunner => "Computer Agent Runner 服务，专注于代理运行和执行",
        }
    }

    /// 获取容器名称前缀
    pub fn container_prefix(&self) -> &str {
        match self {
            ServiceType::RCoder => "rcoder-agent",
            ServiceType::ComputerAgentRunner => "computer-agent-runner",
        }
    }

    /// 检查服务是否在给定的多镜像配置中启用
    pub fn is_enabled(&self, config: &crate::MultiImageConfig) -> bool {
        let service_key = self.to_string();
        if let Some(service_config) = config.services.get(&service_key) {
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
    #[error("不支持的服务类型 '{0}'，请使用 'rcoder' 或 'computer-agent-runner'")]
    InvalidServiceType(String),
    #[error("服务类型 '{0}' 已禁用")]
    ServiceDisabled(String),
}

/// 获取所有支持的服务类型
pub fn get_supported_service_types() -> Vec<String> {
    vec!["rcoder".to_string(), "computer-agent-runner".to_string()]
}

/// 获取启用的服务类型列表
pub fn get_enabled_service_types(config: &crate::MultiImageConfig) -> Vec<String> {
    let supported = get_supported_service_types();
    supported
        .into_iter()
        .filter_map(|service_type| {
            // 使用 parse() 替代 from_str()
            match service_type.parse::<ServiceType>() {
                Ok(service) => {
                    if service.is_enabled(config) {
                        Some(service_type)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::warn!("解析服务类型失败: {} - {:?}", service_type, e);
                    None
                }
            }
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
                },
                work_dir: "/app".to_string(),
                network_mode: "bridge".to_string(),
                container_path_template: "/app/project_workspace/{project_id}".to_string(),
            },
        );

        services.insert(
            "computer-agent-runner".to_string(),
            ServiceImageConfig {
                service_type: ServiceType::ComputerAgentRunner,
                image: None,
                arm64_image: Some("registry.yichamao.com/computer-agent-runner:arm64".to_string()),
                amd64_image: Some("registry.yichamao.com/computer-agent-runner:amd64".to_string()),
                default_image: None,
                image_tag_prefix: Some("computer-agent-runner".to_string()),
                enabled: false, // 默认禁用
                environment: HashMap::new(),
                mounts: vec![],
                command: vec![],
                entrypoint: None,
                resource_limits: ServiceResourceLimits {
                    memory_limit: None,
                    cpu_limit: None,
                    swap_limit: None,
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
        assert_eq!(ServiceType::RCoder.to_string(), "rcoder");
        assert_eq!(
            ServiceType::ComputerAgentRunner.to_string(),
            "computer-agent-runner"
        );

        assert!(ServiceType::RCoder.description().contains("完整"));
        assert!(
            ServiceType::ComputerAgentRunner
                .description()
                .contains("执行")
        );
    }

    #[test]
    fn test_service_type_from_str() {
        // 有效的服务类型
        assert_eq!(
            "rcoder".parse::<ServiceType>().unwrap(),
            ServiceType::RCoder
        );
        assert_eq!(
            "computer-agent-runner".parse::<ServiceType>().unwrap(),
            ServiceType::ComputerAgentRunner
        );

        // 未知类型应该返回错误
        assert!("unknown".parse::<ServiceType>().is_err());

        // 空字符串应该返回错误
        assert!("".parse::<ServiceType>().is_err());
        assert!("   ".parse::<ServiceType>().is_err());
    }

    #[test]
    fn test_service_type_enabled() {
        let config = create_test_config();

        // RCoder 应该启用
        assert!(ServiceType::RCoder.is_enabled(&config));

        // ComputerAgentRunner 应该禁用
        assert!(!ServiceType::ComputerAgentRunner.is_enabled(&config));
    }

    #[test]
    fn test_get_supported_service_types() {
        let types = get_supported_service_types();
        assert_eq!(types.len(), 2);
        assert!(types.contains(&"rcoder".to_string()));
        assert!(types.contains(&"computer-agent-runner".to_string()));
    }

    #[test]
    fn test_get_enabled_service_types() {
        let config = create_test_config();
        let enabled = get_enabled_service_types(&config);

        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains(&"rcoder".to_string()));
        assert!(!enabled.contains(&"computer-agent-runner".to_string()));
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
        ServiceType::ComputerAgentRunner.hash(&mut hasher2);

        assert_ne!(hasher1.finish(), hasher2.finish());
    }
}

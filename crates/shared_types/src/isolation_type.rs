//! 隔离类型定义
//!
//! 定义了容器隔离级别的枚举类型，用于支持多租户场景下的数据隔离。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// 隔离类型枚举
///
/// 决定了容器共享的粒度和数据目录结构：
/// - Tenant: 租户隔离，同一租户下的所有用户共享容器
/// - Space: 空间隔离，同一租户同一空间下的用户共享容器
/// - Project: 项目隔离，每个项目独立容器（默认行为）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[derive(Default)]
pub enum IsolationType {
    /// 租户隔离：同一租户共用一个容器
    Tenant,
    /// 空间隔离：同一租户同一空间共用一个容器
    Space,
    /// 项目隔离：每个项目独立容器（当前默认逻辑）
    #[default]
    Project,
}


impl IsolationType {
    /// 从字符串解析隔离类型
    pub fn from_str(s: &str) -> Result<Self, IsolationTypeError> {
        match s.to_lowercase().as_str() {
            "tenant" => Ok(IsolationType::Tenant),
            "space" => Ok(IsolationType::Space),
            "project" => Ok(IsolationType::Project),
            _ => Err(IsolationTypeError::InvalidIsolationType(s.to_string())),
        }
    }

    /// 获取隔离类型的字符串表示
    pub fn as_str(&self) -> &'static str {
        match self {
            IsolationType::Tenant => "tenant",
            IsolationType::Space => "space",
            IsolationType::Project => "project",
        }
    }

    /// 检查是否为多级路径隔离（tenant 或 space）
    pub fn is_multi_level(&self) -> bool {
        matches!(self, IsolationType::Tenant | IsolationType::Space)
    }
}

/// 隔离类型解析错误
#[derive(Debug, Error)]
pub enum IsolationTypeError {
    #[error("invalid isolation_type: {0}, expected tenant|space|project")]
    InvalidIsolationType(String),
}

impl std::fmt::Display for IsolationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_str_valid() {
        assert_eq!(IsolationType::from_str("tenant").unwrap(), IsolationType::Tenant);
        assert_eq!(IsolationType::from_str("space").unwrap(), IsolationType::Space);
        assert_eq!(IsolationType::from_str("project").unwrap(), IsolationType::Project);

        // 大小写不敏感
        assert_eq!(IsolationType::from_str("TENANT").unwrap(), IsolationType::Tenant);
        assert_eq!(IsolationType::from_str("Space").unwrap(), IsolationType::Space);
    }

    #[test]
    fn test_from_str_invalid() {
        assert!(IsolationType::from_str("invalid").is_err());
        assert!(IsolationType::from_str("").is_err());
    }

    #[test]
    fn test_as_str() {
        assert_eq!(IsolationType::Tenant.as_str(), "tenant");
        assert_eq!(IsolationType::Space.as_str(), "space");
        assert_eq!(IsolationType::Project.as_str(), "project");
    }

    #[test]
    fn test_is_multi_level() {
        assert!(IsolationType::Tenant.is_multi_level());
        assert!(IsolationType::Space.is_multi_level());
        assert!(!IsolationType::Project.is_multi_level());
    }

    #[test]
    fn test_default() {
        assert_eq!(IsolationType::default(), IsolationType::Project);
    }
}

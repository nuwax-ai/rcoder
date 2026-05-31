//! Computer Use 模式的路径常量和辅助函数
//!
//! 统一管理容器内的项目工作空间路径，避免硬编码分散在各处。
//! 所有标识符（user_id, project_id, tenant_id, space_id）在使用前必须通过验证，
//! 防止路径穿越和注入攻击。

use std::sync::LazyLock;

use regex::Regex;

/// 容器内 RCoder 项目工作空间的根目录
///
/// 容器内的目录结构（isolation_type=project）：
/// ```text
/// /app/project_workspace/
/// └── {project_id}/
///     └── (项目文件)
/// ```
///
/// 容器内的目录结构（isolation_type=tenant/space）：
/// ```text
/// /app/project_workspace/
/// └── {tenant_id}/
///     └── {space_id}/
///         └── {project_id}/
///             └── (项目文件)
/// ```
pub const WORKSPACE_ROOT: &str = "/app/project_workspace";

/// 容器内 Computer Use 项目工作空间的根目录
///
/// 容器内的目录结构（isolation_type=project）：
/// ```text
/// /app/computer-project-workspace/
/// └── {user_id}/
///     └── {project_id}/
///         └── (项目文件)
/// ```
///
/// 容器内的目录结构（isolation_type=tenant/space）：
/// ```text
/// /app/computer-project-workspace/
/// └── {tenant_id}/
///     └── {space_id}/
///         └── {project_id}/
///             └── (项目文件)
/// ```
pub const COMPUTER_WORKSPACE_ROOT: &str = "/app/computer-project-workspace";

/// 标识符验证正则：仅允许字母、数字、下划线、连字符
/// 长度 1-64 字符
static IDENTIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").expect("identifier regex is valid")
});

/// 路径标识符验证错误
#[derive(Debug, thiserror::Error)]
pub enum PathValidationError {
    #[error("{field} 不能为空")]
    Empty { field: String },
    #[error("{field} 包含非法字符: '{value}'，仅允许字母、数字、下划线和连字符，长度 1-64")]
    Invalid { field: String, value: String },
}

/// 验证路径标识符（user_id, project_id, tenant_id, space_id 等）
///
/// # 规则
/// - 仅允许 `[a-zA-Z0-9_-]`
/// - 长度 1-64 字符
/// - 不允许 `.`（防止路径穿越）
/// - 不允许 `/`（防止路径注入）
/// - 不允许空字符串
///
/// # 错误
/// 返回 `PathValidationError` 包含中文错误描述
pub fn validate_identifier(value: &str, field_name: &str) -> Result<(), PathValidationError> {
    if value.is_empty() {
        return Err(PathValidationError::Empty {
            field: field_name.to_string(),
        });
    }
    if !IDENTIFIER_RE.is_match(value) {
        return Err(PathValidationError::Invalid {
            field: field_name.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

/// 构建用户目录路径（Computer Use 模式）
///
/// # 示例
/// ```ignore
/// let path = user_dir("user123").unwrap();
/// assert_eq!(path, "/app/computer-project-workspace/user123");
/// ```
///
/// # 错误
/// 当 `user_id` 包含非法字符时返回 `PathValidationError`
pub fn user_dir(user_id: &str) -> Result<String, PathValidationError> {
    validate_identifier(user_id, "user_id")?;
    Ok(format!("{}/{}", COMPUTER_WORKSPACE_ROOT, user_id))
}

/// 构建项目目录路径（Computer Use 模式，project 隔离）
///
/// # 示例
/// ```ignore
/// let path = project_dir("user123", "project456").unwrap();
/// assert_eq!(path, "/app/computer-project-workspace/user123/project456");
/// ```
///
/// # 错误
/// 当 `user_id` 或 `project_id` 包含非法字符时返回 `PathValidationError`
pub fn project_dir(user_id: &str, project_id: &str) -> Result<String, PathValidationError> {
    validate_identifier(user_id, "user_id")?;
    validate_identifier(project_id, "project_id")?;
    Ok(format!(
        "{}/{}/{}",
        COMPUTER_WORKSPACE_ROOT, user_id, project_id
    ))
}

/// 根据隔离类型构建 RCoder 工作空间路径
///
/// # 参数
/// - `isolation_type`: 隔离类型，可选值为 "tenant"、"space"、"project"
/// - `tenant_id`: 租户 ID（当 isolation_type 为 tenant 或 space 时必需）
/// - `space_id`: 空间 ID（当 isolation_type 为 tenant 或 space 时必需）
/// - `project_id`: 项目 ID（必需）
///
/// # 返回
/// 拼接后的容器内路径
///
/// # 示例
/// ```ignore
/// // project 隔离（默认）
/// build_workspace_path(Some("project"), None, None, "proj_123").unwrap()
/// // 返回: "/app/project_workspace/proj_123"
///
/// // tenant 隔离
/// build_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "proj_123").unwrap()
/// // 返回: "/app/project_workspace/t1/s1/proj_123"
/// ```
///
/// # 错误
/// 当任何标识符包含非法字符时返回 `PathValidationError`
pub fn build_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    project_id: &str,
) -> Result<String, PathValidationError> {
    validate_identifier(project_id, "project_id")?;

    // 大小写不敏感：统一转小写后匹配
    let normalized = isolation_type.map(|s| s.to_lowercase());
    match normalized.as_deref() {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/project_workspace/{tenant_id}/{space_id}/{project_id}
            let tid = tenant_id.unwrap_or("default");
            let sid = space_id.unwrap_or("default");
            validate_identifier(tid, "tenant_id")?;
            validate_identifier(sid, "space_id")?;
            Ok(format!("{}/{}/{}/{}", WORKSPACE_ROOT, tid, sid, project_id))
        }
        _ => {
            // project (默认): /app/project_workspace/{project_id}
            Ok(format!("{}/{}", WORKSPACE_ROOT, project_id))
        }
    }
}

/// 根据隔离类型构建 Computer 工作空间路径
///
/// # 参数
/// - `isolation_type`: 隔离类型，可选值为 "tenant"、"space"、"project"
/// - `tenant_id`: 租户 ID（当 isolation_type 为 tenant 或 space 时必需）
/// - `space_id`: 空间 ID（当 isolation_type 为 tenant 或 space 时必需）
/// - `user_id`: 用户 ID（当 isolation_type 为 project 时使用）
/// - `project_id`: 项目 ID（必需）
///
/// # 返回
/// 拼接后的容器内路径
///
/// # 示例
/// ```ignore
/// // project 隔离（默认）
/// build_computer_workspace_path(Some("project"), None, None, "user_123", "proj_456").unwrap()
/// // 返回: "/app/computer-project-workspace/user_123/proj_456"
///
/// // tenant 隔离
/// build_computer_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "user_123", "proj_456").unwrap()
/// // 返回: "/app/computer-project-workspace/t1/s1/proj_456"
/// ```
///
/// # 错误
/// 当任何标识符包含非法字符时返回 `PathValidationError`
pub fn build_computer_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    user_id: &str,
    project_id: &str,
) -> Result<String, PathValidationError> {
    validate_identifier(project_id, "project_id")?;

    // 大小写不敏感：统一转小写后匹配
    let normalized = isolation_type.map(|s| s.to_lowercase());
    match normalized.as_deref() {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}
            let tid = tenant_id.unwrap_or("default");
            let sid = space_id.unwrap_or("default");
            validate_identifier(tid, "tenant_id")?;
            validate_identifier(sid, "space_id")?;
            Ok(format!(
                "{}/{}/{}/{}",
                COMPUTER_WORKSPACE_ROOT, tid, sid, project_id
            ))
        }
        _ => {
            // project (默认): /app/computer-project-workspace/{user_id}/{project_id}
            validate_identifier(user_id, "user_id")?;
            Ok(format!(
                "{}/{}/{}",
                COMPUTER_WORKSPACE_ROOT, user_id, project_id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === validate_identifier 测试 ===

    #[test]
    fn test_validate_identifier_accepts_valid() {
        assert!(validate_identifier("user123", "user_id").is_ok());
        assert!(validate_identifier("my-project_01", "project_id").is_ok());
        assert!(validate_identifier("a", "id").is_ok());
        assert!(validate_identifier("A-B_C", "id").is_ok());
        assert!(validate_identifier("12345", "id").is_ok());
    }

    #[test]
    fn test_validate_identifier_rejects_traversal() {
        assert!(validate_identifier("../etc", "user_id").is_err());
        assert!(validate_identifier("..\\etc", "user_id").is_err());
        assert!(validate_identifier("foo/bar", "user_id").is_err());
        assert!(validate_identifier("..", "user_id").is_err());
        assert!(validate_identifier(".", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_special_chars() {
        assert!(validate_identifier("user id", "user_id").is_err());
        assert!(validate_identifier("user;rm", "user_id").is_err());
        assert!(validate_identifier("user$id", "user_id").is_err());
        assert!(validate_identifier("user@id", "user_id").is_err());
        assert!(validate_identifier("user\nid", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_empty() {
        assert!(validate_identifier("", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_too_long() {
        let long_id = "a".repeat(65);
        assert!(validate_identifier(&long_id, "user_id").is_err());
        // 64 chars should be ok
        let ok_id = "a".repeat(64);
        assert!(validate_identifier(&ok_id, "user_id").is_ok());
    }

    // === 路径构建函数测试 ===

    #[test]
    fn test_user_dir() {
        assert_eq!(
            user_dir("user123").unwrap(),
            "/app/computer-project-workspace/user123"
        );
    }

    #[test]
    fn test_user_dir_rejects_traversal() {
        assert!(user_dir("../../etc").is_err());
    }

    #[test]
    fn test_project_dir() {
        assert_eq!(
            project_dir("user123", "project456").unwrap(),
            "/app/computer-project-workspace/user123/project456"
        );
    }

    #[test]
    fn test_project_dir_rejects_traversal() {
        assert!(project_dir("user123", "../../etc").is_err());
        assert!(project_dir("../../etc", "project456").is_err());
    }

    #[test]
    fn test_build_workspace_path_project() {
        // project 隔离（默认）
        assert_eq!(
            build_workspace_path(None, None, None, "proj_123").unwrap(),
            "/app/project_workspace/proj_123"
        );
        assert_eq!(
            build_workspace_path(Some("project"), None, None, "proj_123").unwrap(),
            "/app/project_workspace/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_tenant() {
        // tenant 隔离
        assert_eq!(
            build_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "proj_123").unwrap(),
            "/app/project_workspace/t1/s1/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_space() {
        // space 隔离
        assert_eq!(
            build_workspace_path(Some("space"), Some("t1"), Some("s1"), "proj_123").unwrap(),
            "/app/project_workspace/t1/s1/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_defaults() {
        // tenant/space 模式下使用默认值
        assert_eq!(
            build_workspace_path(Some("tenant"), None, None, "proj_123").unwrap(),
            "/app/project_workspace/default/default/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_rejects_traversal() {
        assert!(build_workspace_path(None, None, None, "../../etc").is_err());
        assert!(build_workspace_path(Some("tenant"), Some("../bad"), Some("s1"), "proj").is_err());
    }

    #[test]
    fn test_build_computer_workspace_path_project() {
        // project 隔离（默认）
        assert_eq!(
            build_computer_workspace_path(None, None, None, "user_123", "proj_456").unwrap(),
            "/app/computer-project-workspace/user_123/proj_456"
        );
        assert_eq!(
            build_computer_workspace_path(Some("project"), None, None, "user_123", "proj_456")
                .unwrap(),
            "/app/computer-project-workspace/user_123/proj_456"
        );
    }

    #[test]
    fn test_build_computer_workspace_path_tenant() {
        // tenant 隔离
        assert_eq!(
            build_computer_workspace_path(
                Some("tenant"),
                Some("t1"),
                Some("s1"),
                "user_123",
                "proj_456"
            )
            .unwrap(),
            "/app/computer-project-workspace/t1/s1/proj_456"
        );
    }

    #[test]
    fn test_build_computer_workspace_path_space() {
        // space 隔离
        assert_eq!(
            build_computer_workspace_path(
                Some("space"),
                Some("t1"),
                Some("s1"),
                "user_123",
                "proj_456"
            )
            .unwrap(),
            "/app/computer-project-workspace/t1/s1/proj_456"
        );
    }

    #[test]
    fn test_build_computer_workspace_path_rejects_traversal() {
        assert!(build_computer_workspace_path(None, None, None, "../../etc", "proj").is_err());
        assert!(build_computer_workspace_path(None, None, None, "user", "../../etc").is_err());
    }
}

//! Computer Use 模式的路径常量和辅助函数
//!
//! 统一管理容器内的项目工作空间路径，避免硬编码分散在各处。

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

/// 构建用户目录路径（Computer Use 模式）
///
/// # 示例
/// ```ignore
/// let path = user_dir("user123");
/// assert_eq!(path, "/app/computer-project-workspace/user123");
/// ```
pub fn user_dir(user_id: &str) -> String {
    format!("{}/{}", COMPUTER_WORKSPACE_ROOT, user_id)
}

/// 构建项目目录路径（Computer Use 模式，project 隔离）
///
/// # 示例
/// ```ignore
/// let path = project_dir("user123", "project456");
/// assert_eq!(path, "/app/computer-project-workspace/user123/project456");
/// ```
pub fn project_dir(user_id: &str, project_id: &str) -> String {
    format!("{}/{}/{}", COMPUTER_WORKSPACE_ROOT, user_id, project_id)
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
/// build_workspace_path(Some("project"), None, None, "proj_123")
/// // 返回: "/app/project_workspace/proj_123"
///
/// // tenant 隔离
/// build_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "proj_123")
/// // 返回: "/app/project_workspace/t1/s1/proj_123"
/// ```
pub fn build_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    project_id: &str,
) -> String {
    // 大小写不敏感：统一转小写后匹配
    let normalized = isolation_type.map(|s| s.to_lowercase());
    match normalized.as_deref() {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/project_workspace/{tenant_id}/{space_id}/{project_id}
            let tid = tenant_id.unwrap_or("default");
            let sid = space_id.unwrap_or("default");
            format!("{}/{}/{}/{}", WORKSPACE_ROOT, tid, sid, project_id)
        }
        _ => {
            // project (默认): /app/project_workspace/{project_id}
            format!("{}/{}", WORKSPACE_ROOT, project_id)
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
/// build_computer_workspace_path(Some("project"), None, None, "user_123", "proj_456")
/// // 返回: "/app/computer-project-workspace/user_123/proj_456"
///
/// // tenant 隔离
/// build_computer_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "user_123", "proj_456")
/// // 返回: "/app/computer-project-workspace/t1/s1/proj_456"
/// ```
pub fn build_computer_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    user_id: &str,
    project_id: &str,
) -> String {
    // 大小写不敏感：统一转小写后匹配
    let normalized = isolation_type.map(|s| s.to_lowercase());
    match normalized.as_deref() {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}
            let tid = tenant_id.unwrap_or("default");
            let sid = space_id.unwrap_or("default");
            format!("{}/{}/{}/{}", COMPUTER_WORKSPACE_ROOT, tid, sid, project_id)
        }
        _ => {
            // project (默认): /app/computer-project-workspace/{user_id}/{project_id}
            format!("{}/{}/{}", COMPUTER_WORKSPACE_ROOT, user_id, project_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_dir() {
        assert_eq!(
            user_dir("user123"),
            "/app/computer-project-workspace/user123"
        );
    }

    #[test]
    fn test_project_dir() {
        assert_eq!(
            project_dir("user123", "project456"),
            "/app/computer-project-workspace/user123/project456"
        );
    }

    #[test]
    fn test_build_workspace_path_project() {
        // project 隔离（默认）
        assert_eq!(
            build_workspace_path(None, None, None, "proj_123"),
            "/app/project_workspace/proj_123"
        );
        assert_eq!(
            build_workspace_path(Some("project"), None, None, "proj_123"),
            "/app/project_workspace/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_tenant() {
        // tenant 隔离
        assert_eq!(
            build_workspace_path(Some("tenant"), Some("t1"), Some("s1"), "proj_123"),
            "/app/project_workspace/t1/s1/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_space() {
        // space 隔离
        assert_eq!(
            build_workspace_path(Some("space"), Some("t1"), Some("s1"), "proj_123"),
            "/app/project_workspace/t1/s1/proj_123"
        );
    }

    #[test]
    fn test_build_workspace_path_defaults() {
        // tenant/space 模式下使用默认值
        assert_eq!(
            build_workspace_path(Some("tenant"), None, None, "proj_123"),
            "/app/project_workspace/default/default/proj_123"
        );
    }

    #[test]
    fn test_build_computer_workspace_path_project() {
        // project 隔离（默认）
        assert_eq!(
            build_computer_workspace_path(None, None, None, "user_123", "proj_456"),
            "/app/computer-project-workspace/user_123/proj_456"
        );
        assert_eq!(
            build_computer_workspace_path(Some("project"), None, None, "user_123", "proj_456"),
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
            ),
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
            ),
            "/app/computer-project-workspace/t1/s1/proj_456"
        );
    }
}

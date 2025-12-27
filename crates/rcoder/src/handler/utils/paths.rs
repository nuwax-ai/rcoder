//! Computer Use 模式的路径常量和辅助函数
//!
//! 统一管理容器内的项目工作空间路径，避免硬编码分散在各处。

/// 容器内 Computer Use 项目工作空间的根目录
///
/// 容器内的目录结构：
/// ```text
/// /app/computer-project-workspace/
/// └── {user_id}/
///     └── {project_id}/
///         └── (项目文件)
/// ```
pub const COMPUTER_WORKSPACE_ROOT: &str = "/app/computer-project-workspace";

/// 构建用户目录路径
///
/// # 示例
/// ```ignore
/// let path = user_dir("user123");
/// assert_eq!(path, "/app/computer-project-workspace/user123");
/// ```
pub fn user_dir(user_id: &str) -> String {
    format!("{}/{}", COMPUTER_WORKSPACE_ROOT, user_id)
}

/// 构建项目目录路径
///
/// # 示例
/// ```ignore
/// let path = project_dir("user123", "project456");
/// assert_eq!(path, "/app/computer-project-workspace/user123/project456");
/// ```
pub fn project_dir(user_id: &str, project_id: &str) -> String {
    format!("{}/{}/{}", COMPUTER_WORKSPACE_ROOT, user_id, project_id)
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
}

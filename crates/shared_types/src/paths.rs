//! 工作区路径常量 (单一事实源, 所有 crate 共用)。
//!
//! 统一管理容器内的工作区根目录, 避免 `/app/project_workspace`、
//! `/app/computer-project-workspace` 散落在 rcoder / docker_manager / agent_runner
//! 各自重复定义。路径拼接逻辑 (含 isolation_type 分支) 见
//! `rcoder::handler::utils::paths::{build_workspace_path, build_computer_workspace_path}`。

/// web agent 工作区根目录。
///
/// rcoder 主容器与 web agent sub-container 都挂此路径 (共享 PVC, subPath=workspace)。
///
/// 目录结构 (isolation=project, 默认):
/// ```text
/// /app/project_workspace/{project_id}/
/// ```
///
/// 目录结构 (isolation=tenant/space, 共享容器):
/// ```text
/// /app/project_workspace/{tenant_id}/{space_id}/{project_id}/
/// ```
pub const WORKSPACE_ROOT: &str = "/app/project_workspace";

/// computer agent 工作区根目录 (**rcoder 主容器视角**)。
///
/// 注意: computer agent sub-container 的挂载点是 `/home/user`
/// (= `{COMPUTER_WORKSPACE_ROOT}/{user_id}`), 不是此根路径本身。
/// rcoder 主容器通过此根访问所有 user 子目录 (挂载共享 PVC 根)。
///
/// 目录结构 (per-user, 与 subPath=user_id 挂载边界对齐):
/// ```text
/// /app/computer-project-workspace/{user_id}/
/// ```
///
/// 目录结构 (per-project):
/// ```text
/// /app/computer-project-workspace/{user_id}/{project_id}/
/// ```
pub const COMPUTER_WORKSPACE_ROOT: &str = "/app/computer-project-workspace";

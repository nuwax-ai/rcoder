//! 工作区路径解析 (阶段 1/2 桥梁抽象)。
//!
//! - 阶段 1 [`LocalWorkspaceResolver`]: 读环境变量, 等价 nuwax-file-server 现状
//!   (本地 fs 直读; rcoder 与 file-server 同 pod 挂共享 PVC subPath)。
//! - 阶段 2 `SubvolumeWorkspaceResolver` (后续 task): 经 `ContainerRuntime` 拿
//!   PV `subvolumePath` → `/app/cephfs-root/{subvolumePath}` 聚合访问 agent 数据。
//!
//! 路径规则对齐 nuwax `projectPathUtils.js`:
//! - web/page: tenant+space+isolationType 均非空 → 三级, 否则单级
//!   (对齐 `shouldUseIsolationPath`: 三者都非空才启用; `isolationType` 仅作开关不进路径)
//! - computer/task: 固定二级 (userId/cId)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};

/// env: web/page 工作区根目录 (对齐 nuwax `PROJECT_SOURCE_DIR`)。
const ENV_PROJECT_SOURCE_DIR: &str = "PROJECT_SOURCE_DIR";
/// env: computer/task 工作区根目录 (对齐 nuwax `COMPUTER_WORKSPACE_DIR`)。
const ENV_COMPUTER_WORKSPACE_DIR: &str = "COMPUTER_WORKSPACE_DIR";

/// web/page 工作区定位上下文 (从 HTTP body/query 反序列化)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    /// 项目 ID (必填)。
    pub project_id: String,
    /// 租户 ID (多租户三级目录; 留空走单级)。
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID (多租户三级目录; 留空走单级)。
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型 (多租户开关; 对齐 nuwax `isolationType`, 仅 `tenant+space+isolationType`
    /// 均非空才启用三级, 但自身不进入路径)。
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// computer/task 工作区定位上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputerContext {
    /// 用户 ID。
    pub user_id: String,
    /// 会话/容器 ID (nuwax `cId`)。
    pub cid: String,
}

/// 工作区路径解析抽象 (阶段 1 本地 / 阶段 2 subvolume 聚合)。
///
/// 实现需 `Send + Sync` 以便经 `Arc<dyn WorkspaceResolver>` 注入 axum `AppState`。
pub trait WorkspaceResolver: Send + Sync {
    /// 解析 web/page 项目工作区绝对路径。
    fn resolve_project(&self, ctx: &ProjectContext) -> PathBuf;

    /// 解析 computer/task 工作区绝对路径。
    fn resolve_computer(&self, ctx: &ComputerContext) -> PathBuf;
}

/// 阶段 1 实现: 读环境变量的本地路径解析, 等价 nuwax-file-server 现状。
#[derive(Clone)]
pub struct LocalWorkspaceResolver {
    project_root: PathBuf,
    computer_root: PathBuf,
}

impl LocalWorkspaceResolver {
    /// 从环境变量构造, 缺省回退到 `shared_types::paths` 常量。
    pub fn from_env() -> Self {
        let project_root = std::env::var(ENV_PROJECT_SOURCE_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(WORKSPACE_ROOT));
        let computer_root = std::env::var(ENV_COMPUTER_WORKSPACE_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(COMPUTER_WORKSPACE_ROOT));
        Self {
            project_root,
            computer_root,
        }
    }

    /// 测试专用: 显式指定根目录 (绕过环境变量)。
    #[cfg(test)]
    pub fn new(project_root: PathBuf, computer_root: PathBuf) -> Self {
        Self {
            project_root,
            computer_root,
        }
    }
}

/// 取 trim 后非空的字符串; 空白视为未设置 (对齐 nuwax `normalizeValue`)。
fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

impl WorkspaceResolver for LocalWorkspaceResolver {
    fn resolve_project(&self, ctx: &ProjectContext) -> PathBuf {
        // 对齐 nuwax `shouldUseIsolationPath`: tenant+space+isolationType 均非空才三级
        match (
            non_empty(&ctx.tenant_id),
            non_empty(&ctx.space_id),
            non_empty(&ctx.isolation_type),
        ) {
            (Some(tenant), Some(space), Some(_)) => self
                .project_root
                .join(tenant)
                .join(space)
                .join(&ctx.project_id),
            // 单级: {root}/{project}
            _ => self.project_root.join(&ctx.project_id),
        }
    }

    fn resolve_computer(&self, ctx: &ComputerContext) -> PathBuf {
        // 二级: {root}/{user}/{cid} (对齐 nuwax gitService.js)
        self.computer_root.join(&ctx.user_id).join(&ctx.cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> LocalWorkspaceResolver {
        LocalWorkspaceResolver::new(
            PathBuf::from("/app/project_workspace"),
            PathBuf::from("/app/computer-project-workspace"),
        )
    }

    #[test]
    fn resolve_project_single_level_when_no_isolation() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        };
        assert_eq!(
            r.resolve_project(&ctx),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[test]
    fn resolve_project_three_level_when_all_isolation_fields_present() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: Some("space-b".into()),
            isolation_type: Some("tenant".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx),
            PathBuf::from("/app/project_workspace/tenant-a/space-b/proj-1")
        );
    }

    #[test]
    fn resolve_project_falls_back_single_when_only_tenant() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: None,
            isolation_type: Some("tenant".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[test]
    fn resolve_project_falls_back_single_when_isolation_type_missing() {
        // 对齐 nuwax: tenant+space 非空但 isolationType 空 → 仍单级
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: Some("space-b".into()),
            isolation_type: None,
        };
        assert_eq!(
            r.resolve_project(&ctx),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[test]
    fn resolve_project_falls_back_single_when_blank_isolation() {
        // 空白字符串视为未设置
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("  ".into()),
            space_id: Some("".into()),
            isolation_type: Some("".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[test]
    fn resolve_computer_two_level() {
        let r = resolver();
        let ctx = ComputerContext {
            user_id: "user-1".into(),
            cid: "cid-1".into(),
        };
        assert_eq!(
            r.resolve_computer(&ctx),
            PathBuf::from("/app/computer-project-workspace/user-1/cid-1")
        );
    }
}

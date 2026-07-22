//! 工作区路径解析 (阶段 1/2 桥梁抽象)。
//!
//! - 阶段 1 [`LocalWorkspaceResolver`]: 读环境变量, 等价 nuwax-file-server 现状
//!   (本地 fs 直读; rcoder 与 file-server 同 pod 挂共享 PVC subPath)。
//! - 阶段 2 [`SubvolumeWorkspaceResolver`]: 经 rcoder 注入的 [`WorkspacePathResolver`]
//!   拿 per-agent PVC 的 subvolume 聚合路径 → `{cephfs-root}/{subvolumePath}/{leaf}`。
//!
//! 路径规则对齐 nuwax `projectPathUtils.js`:
//! - web/page: tenant+space+isolationType 均非空 → 三级, 否则单级
//!   (对齐 `shouldUseIsolationPath`: 三者都非空才启用; `isolationType` 仅作开关不进路径)
//! - computer/task: 固定二级 (userId/cId)
//!
//! 阶段 2 (Subvolume) 数据布局: per-agent PVC (CephFS subvolume) 吸收多租户层级 ——
//! project leaf={projectId} (tenant/space 进 PVC 身份), computer leaf={cid} (user_id 进 PVC 身份)。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shared_types::ServiceType;
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};
use tracing::warn;

use crate::error::{AppError, AppResult};

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
/// 方法为 `async` (阶段 2 Subvolume 要 async 调 rcoder 经 K8s API 读 PV subvolumePath)。
#[async_trait]
pub trait WorkspaceResolver: Send + Sync {
    /// 解析 web/page 项目工作区绝对路径。
    async fn resolve_project(&self, ctx: &ProjectContext) -> AppResult<PathBuf>;

    /// 解析 computer/task 工作区绝对路径。
    async fn resolve_computer(&self, ctx: &ComputerContext) -> AppResult<PathBuf>;
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

    /// 显式指定根目录；供配置文件启动和嵌入式 lib 调用。
    pub fn new(project_root: PathBuf, computer_root: PathBuf) -> Self {
        Self {
            project_root,
            computer_root,
        }
    }
}

/// 取 trim 后非空的字符串; 空白视为未设置 (对齐 nuwax `normalizeValue`)。
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn validated_identifier<'a>(value: &'a str, field: &str) -> AppResult<&'a str> {
    let value = value.trim();
    shared_types::validate_identifier(value, field).map_err(AppError::validation)?;
    Ok(value)
}

#[async_trait]
impl WorkspaceResolver for LocalWorkspaceResolver {
    async fn resolve_project(&self, ctx: &ProjectContext) -> AppResult<PathBuf> {
        let project_id = validated_identifier(&ctx.project_id, "projectId")?;
        // 对齐 nuwax `shouldUseIsolationPath`: tenant+space+isolationType 均非空才三级
        let path = match (
            non_empty(ctx.tenant_id.as_deref()),
            non_empty(ctx.space_id.as_deref()),
            non_empty(ctx.isolation_type.as_deref()),
        ) {
            (Some(tenant), Some(space), Some(_)) => self
                .project_root
                .join(validated_identifier(tenant, "tenantId")?)
                .join(validated_identifier(space, "spaceId")?)
                .join(project_id),
            // 单级: {root}/{project}
            _ => self.project_root.join(project_id),
        };
        Ok(path)
    }

    async fn resolve_computer(&self, ctx: &ComputerContext) -> AppResult<PathBuf> {
        // 二级: {root}/{user}/{cid} (对齐 nuwax gitService.js)
        Ok(self
            .computer_root
            .join(validated_identifier(&ctx.user_id, "userId")?)
            .join(validated_identifier(&ctx.cid, "cId")?))
    }
}

/// 阶段 2: identifier+service_type → rcoder 可访问 workspace 聚合路径的窄解析器。
///
/// file-server 不依赖 container-runtime-api; 由 rcoder 实现此 trait (内部包
/// `ContainerRuntime::resolve_workspace_path`, 返回 `{cephfs_root}/{subvolumePath}`
/// 完整聚合路径), 经 [`SubvolumeWorkspaceResolver`] 注入。返回 `None` 表示该 runtime
/// 不提供聚合视角 (Docker 模式), 调用方降级到 Local。
#[async_trait]
pub trait WorkspacePathResolver: Send + Sync {
    async fn resolve(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> AppResult<Option<PathBuf>>;

    /// 主动 ensure per-agent PVC + resolve + 检查迁移老数据。
    ///
    /// default: 调 resolve (不 ensure, Docker 模式 / 兼容)。
    /// rcoder ContainerRuntimePathResolver 重写: ensure PVC + resolve + lazy_migrate。
    /// 供 SubvolumeWorkspaceResolver 调用 (替代被动 fallback)。
    async fn ensure_and_resolve(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> AppResult<Option<PathBuf>> {
        self.resolve(identifier, service_type).await
    }
}

/// 阶段 2 实现: 经 rcoder 注入的 [`WorkspacePathResolver`] 拿 per-agent PVC 的 subvolume
/// 聚合路径, 拼上 leaf → `{cephfs-root}/{subvolumePath}/{leaf}`。
///
/// 多租户 tenant/space 被 per-project PVC 吸收 (PVC 身份=project), leaf 不含 tenant/space;
/// computer PVC per-user, leaf 不含 user_id。resolve 返回 None (Docker 模式 / K8s API 抖动 /
/// PVC 未 Bound) → 降级到内置 [`LocalWorkspaceResolver`] (fail-open, 不阻断服务)。
pub struct SubvolumeWorkspaceResolver {
    path_resolver: Arc<dyn WorkspacePathResolver>,
    fallback: LocalWorkspaceResolver,
}

impl SubvolumeWorkspaceResolver {
    /// `path_resolver` 由 rcoder 注入 (包 `ContainerRuntime::resolve_workspace_path`)。
    /// `fallback` 用 [`LocalWorkspaceResolver::from_env`] (Local 语义)。
    pub fn new(path_resolver: Arc<dyn WorkspacePathResolver>) -> Self {
        Self {
            path_resolver,
            fallback: LocalWorkspaceResolver::from_env(),
        }
    }
}

#[async_trait]
impl WorkspaceResolver for SubvolumeWorkspaceResolver {
    async fn resolve_project(&self, ctx: &ProjectContext) -> AppResult<PathBuf> {
        let project_id = validated_identifier(&ctx.project_id, "projectId")?;
        // 回滚开关 false → 直接 fallback Local (共享 PVC, 生产行为)
        if !shared_types::per_agent_pvc_enabled() {
            return self.fallback.resolve_project(ctx).await;
        }
        // 主动 ensure per-agent PVC + 迁移 (不被动 fallback; 失败才降级 Local)
        match self
            .path_resolver
            .ensure_and_resolve(project_id, &ServiceType::WebAgentRunner)
            .await?
        {
            Some(subvolume_base) => Ok(subvolume_base.join(project_id)),
            None => {
                warn!(
                    "SubvolumeWorkspaceResolver: ensure_and_resolve returned None for project {}, \
                     falling back to LocalWorkspaceResolver",
                    project_id
                );
                self.fallback.resolve_project(ctx).await
            }
        }
    }

    async fn resolve_computer(&self, ctx: &ComputerContext) -> AppResult<PathBuf> {
        let user_id = validated_identifier(&ctx.user_id, "userId")?;
        let cid = validated_identifier(&ctx.cid, "cId")?;
        // 回滚开关 false → 直接 fallback Local
        if !shared_types::per_agent_pvc_enabled() {
            return self.fallback.resolve_computer(ctx).await;
        }
        match self
            .path_resolver
            .ensure_and_resolve(user_id, &ServiceType::ComputerAgentRunner)
            .await?
        {
            // {cephfs-root}/{subvolumePath}/{cid} (user_id 被 per-user PVC 吸收)
            Some(subvolume_base) => Ok(subvolume_base.join(cid)),
            None => {
                warn!(
                    "SubvolumeWorkspaceResolver: runtime returned None for user {}, \
                     falling back to LocalWorkspaceResolver",
                    user_id
                );
                self.fallback.resolve_computer(ctx).await
            }
        }
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

    #[tokio::test]
    async fn resolve_project_single_level_when_no_isolation() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        };
        assert_eq!(
            r.resolve_project(&ctx).await.expect("resolve project"),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[tokio::test]
    async fn resolve_project_three_level_when_all_isolation_fields_present() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: Some("space-b".into()),
            isolation_type: Some("tenant".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx).await.expect("resolve project"),
            PathBuf::from("/app/project_workspace/tenant-a/space-b/proj-1")
        );
    }

    #[tokio::test]
    async fn resolve_project_falls_back_single_when_only_tenant() {
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: None,
            isolation_type: Some("tenant".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx).await.expect("resolve project"),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[tokio::test]
    async fn resolve_project_falls_back_single_when_isolation_type_missing() {
        // 对齐 nuwax: tenant+space 非空但 isolationType 空 → 仍单级
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("tenant-a".into()),
            space_id: Some("space-b".into()),
            isolation_type: None,
        };
        assert_eq!(
            r.resolve_project(&ctx).await.expect("resolve project"),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[tokio::test]
    async fn resolve_project_falls_back_single_when_blank_isolation() {
        // 空白字符串视为未设置
        let r = resolver();
        let ctx = ProjectContext {
            project_id: "proj-1".into(),
            tenant_id: Some("  ".into()),
            space_id: Some("".into()),
            isolation_type: Some("".into()),
        };
        assert_eq!(
            r.resolve_project(&ctx).await.expect("resolve project"),
            PathBuf::from("/app/project_workspace/proj-1")
        );
    }

    #[tokio::test]
    async fn resolve_computer_two_level() {
        let r = resolver();
        let ctx = ComputerContext {
            user_id: "user-1".into(),
            cid: "cid-1".into(),
        };
        assert_eq!(
            r.resolve_computer(&ctx).await.expect("resolve computer"),
            PathBuf::from("/app/computer-project-workspace/user-1/cid-1")
        );
    }

    #[tokio::test]
    async fn resolver_rejects_path_traversal_identifiers() {
        let resolver = resolver();
        let project = ProjectContext {
            project_id: "../outside".into(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        };
        assert!(resolver.resolve_project(&project).await.is_err());

        let tenant = ProjectContext {
            project_id: "project".into(),
            tenant_id: Some("../tenant".into()),
            space_id: Some("space".into()),
            isolation_type: Some("tenant".into()),
        };
        assert!(resolver.resolve_project(&tenant).await.is_err());

        let computer = ComputerContext {
            user_id: "user".into(),
            cid: "../../outside".into(),
        };
        assert!(resolver.resolve_computer(&computer).await.is_err());
    }
}

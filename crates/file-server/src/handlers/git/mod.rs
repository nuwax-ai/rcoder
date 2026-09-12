//! `/api/git` HTTP handlers (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。
//!
//! 拆分: [`read`] (branches / tags / log / file-content / status) / [`write`]
//! (init / add / commit / unstage / discard / diff / reset / checkout / revert) /
//! [`refs`] (branch-create / branch-delete / branch-switch / tag-create / tag-delete)。
//! 本 mod.rs 提供共享路径解析 helper（serviceContext 优先 + workspaceType 老规则，
//! 对齐 TS 6321f7e `resolveAndCheck`）；共享 base 结构 (GitQuery / GitWriteBody)
//! 定义在 [`crate::models`]。

use std::path::PathBuf;

use shared_types::ComputerServiceKind;

use crate::AppState;
use crate::error::AppError;
use crate::models::{GitQuery, GitWriteBody};
use crate::service::git;
use crate::workspace::{ComputerContext, ProjectContext};

pub(crate) mod read;
pub(crate) mod refs;
pub(crate) mod write;

// ── 共享 base 结构 + 路径解析 (子模块经 super:: 访问) ─────────────────────────────

fn project_ctx(q: &GitQuery) -> Option<ProjectContext> {
    Some(ProjectContext {
        project_id: q.project_id.clone()?,
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

fn computer_ctx(q: &GitQuery) -> Option<ComputerContext> {
    Some(ComputerContext {
        user_id: q.user_id.clone()?,
        cid: q.c_id.clone()?,
    })
}

/// git 族 serviceContext 三要素（对齐 TS 6321f7e gitRoutes `extractServiceContext`
/// → `resolveServiceContext` 的合并产物）。
///
/// 合并序：header（task-local，中间件已 scope）> body/query 字段。
/// 构造失败（userapp/normalProject 场景缺 appId——TS 侧 `resolveServiceContext`
/// throw 被 `extractServiceContext` catch 置 null）返回 `None`，调用方回落
/// workspaceType 老规则（同构不 fail-fast）。
#[derive(Debug, Default)]
pub(super) struct GitServiceContext {
    kind: Option<ComputerServiceKind>,
    app_id: Option<String>,
    workspace_path: Option<String>,
}

impl GitServiceContext {
    /// 合并 header 通道（task-local）与 body/query 通道。
    ///
    /// kind：header 归一结果（`None`=header 无有效值）优先，未匹配轮到 body/query
    /// 的 `serviceType` 字段归一（对齐 TS `headerType || bodyType` 链——header
    /// 未匹配视为无值而非缺省）。
    ///
    /// appId/workspacePath 的 body 值统一 trim 非空过滤（对齐 TS
    /// `String(x).trim() || null`——空白串视为未传，防「假激活」导致与 TS
    /// 分歧：TS 对空白值不激活 serviceContext 走 workspaceType 老规则）。
    fn merge(
        body_service_type: Option<&str>,
        body_app_id: Option<&str>,
        body_workspace_path: Option<&str>,
    ) -> Option<Self> {
        let kind = crate::extract::service_kind()
            .or_else(|| body_service_type.and_then(shared_types::normalize_computer_service_type));
        // appId：task-local（header x-app-id > query appId，读取器已 trim 过滤）
        // 优先，兜底 body 字段
        let app_id = crate::extract::userapp_app_id()
            .or_else(|| crate::extract::non_empty_trimmed(body_app_id).map(str::to_string));
        // workspacePath 的 header > 显式值合并由收口核心的 merged_workspace_path 做
        let workspace_path =
            crate::extract::non_empty_trimmed(body_workspace_path).map(str::to_string);
        match kind {
            // userapp/normalProject 缺 appId = TS 构造失败 → 整体视为无 serviceContext
            Some(ComputerServiceKind::Userapp | ComputerServiceKind::NormalProject)
                if app_id.is_none() =>
            {
                None
            }
            kind => Some(Self {
                kind,
                app_id,
                workspace_path,
            }),
        }
    }

    /// serviceContext 分支激活条件（TS：`workspacePath || appId` 任一存在）。
    fn active(&self) -> bool {
        self.workspace_path.is_some() || self.app_id.is_some()
    }
}

/// git 族定位收口：serviceContext 分支（优先）+ workspaceType 老规则回落。
///
/// serviceContext 激活时（对齐 TS `resolveAndCheck`）：
/// - userId/cId 必填（来自 git 参数三元组，缺一 `Validation` 拒绝）
/// - 定位复用 [`crate::handlers::computer::computer_root_for_context`]（与
///   computer 域同构：workspacePath 绑定 > userapp 开发卷 > normalProject
///   共享工作区 > taskAgent 默认）
/// - 目标目录不存在 → `Resource` 错（"Workspace does not exist"，TS 同款——
///   git 操作只面向已存在工作区，不创建）
async fn resolve_target(
    state: &AppState,
    workspace_type: &str,
    project_ctx: Option<&ProjectContext>,
    computer_ctx: Option<&ComputerContext>,
    service: &GitServiceContext,
) -> Result<(PathBuf, String), AppError> {
    if service.active() {
        let ctx = computer_ctx
            .filter(|ctx| !ctx.user_id.trim().is_empty() && !ctx.cid.trim().is_empty())
            .ok_or_else(|| AppError::validation("serviceContext mode requires userId and cId"))?;
        let path = crate::handlers::computer::computer_root_for_context(
            state,
            &ctx.user_id,
            &ctx.cid,
            service.kind,
            service.app_id.as_deref(),
            service.workspace_path.as_deref(),
        )
        .await?;
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(AppError::resource("Workspace does not exist"));
        }
        // logId 与 taskAgent 老规则同形态（TS：`computer:{userId}:{cId}`）
        return Ok((path, format!("computer:{}:{}", ctx.user_id, ctx.cid)));
    }
    let target =
        git::resolve_target(&*state.resolver, workspace_type, project_ctx, computer_ctx).await?;
    Ok((target.path().to_path_buf(), target.log_id()))
}

/// GET 路由解析: GitQuery → (workspace path, logId)。
pub(super) async fn resolve(q: &GitQuery, state: &AppState) -> Result<(PathBuf, String), AppError> {
    let service = GitServiceContext::merge(
        q.service_type.as_deref(),
        q.app_id.as_deref(),
        q.workspace_path.as_deref(),
    )
    // serviceContext 构造失败（None）= 回落 workspaceType 老规则，与未携带同路径
    .unwrap_or_default();
    resolve_target(
        state,
        q.workspace_type.as_deref().unwrap_or(""),
        project_ctx(q).as_ref(),
        computer_ctx(q).as_ref(),
        &service,
    )
    .await
}

/// POST 路由解析: GitWriteBody → (workspace path, logId)。
pub(super) async fn resolve_body(
    state: &AppState,
    body: &GitWriteBody,
) -> Result<(PathBuf, String), AppError> {
    let project_ctx = body.project_id.clone().map(|id| ProjectContext {
        project_id: id,
        tenant_id: body.tenant_id.clone(),
        space_id: body.space_id.clone(),
        isolation_type: body.isolation_type.clone(),
    });
    let computer_ctx = match (&body.user_id, &body.c_id) {
        (Some(u), Some(c)) => Some(ComputerContext {
            user_id: u.clone(),
            cid: c.clone(),
        }),
        _ => None,
    };
    let service = GitServiceContext::merge(
        body.service_type.as_deref(),
        body.app_id.as_deref(),
        body.workspace_path.as_deref(),
    )
    .unwrap_or_default();
    resolve_target(
        state,
        &body.workspace_type,
        project_ctx.as_ref(),
        computer_ctx.as_ref(),
        &service,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::error::AppResult;
    use crate::workspace::{ProjectContext, WorkspaceResolver};

    /// resolver 根指向的目录由各测试自行预建（git 老规则定位要求存在）。
    struct FixedResolver {
        root: PathBuf,
    }

    #[async_trait]
    impl WorkspaceResolver for FixedResolver {
        async fn resolve_project(&self, ctx: &ProjectContext) -> AppResult<PathBuf> {
            Ok(self.root.join("project").join(&ctx.project_id))
        }

        async fn resolve_computer(&self, ctx: &ComputerContext) -> AppResult<PathBuf> {
            Ok(self.root.join(&ctx.user_id).join(&ctx.cid))
        }
    }

    thread_local! {
        static RESOLVER_ROOT: PathBuf = std::env::temp_dir().join(format!(
            "git-e2e-resolver-{}",
            std::process::id()
        ));
    }

    fn make_state() -> AppState {
        RESOLVER_ROOT.with(|root| {
            std::fs::create_dir_all(root.join("u1").join("c1")).expect("seed resolver workspace");
            let config = Arc::new(crate::Config::default());
            AppState {
                resolver: Arc::new(FixedResolver { root: root.clone() }),
                dev_server: Arc::new(crate::DevServerManager::new(config.clone())),
                build_manager: Arc::new(crate::BuildManager::new(config.max_build_concurrency)),
                log_cache: Arc::new(crate::LogCacheManager::new(&config)),
                skill_downloader: Arc::new(
                    crate::SkillDownloader::new(&config).expect("construct skill downloader"),
                ),
                config,
                started_at: std::time::Instant::now(),
            }
        })
    }

    /// task-local 三要素 scope 助手（模拟中间件 header 通道注入）。
    async fn scope_context<F, T>(kind: Option<ComputerServiceKind>, app_id: Option<&str>, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let app_id = app_id.map(str::to_string);
        crate::extract::SERVICE_KIND
            .scope(kind, async move {
                crate::extract::USERAPP_APP_ID.scope(app_id, f).await
            })
            .await
    }

    fn computer_ctx(user: &str, cid: &str) -> Option<ComputerContext> {
        Some(ComputerContext {
            user_id: user.into(),
            cid: cid.into(),
        })
    }

    /// serviceContext 激活时优先级覆盖 workspaceType 老规则：header userapp +
    /// appId 落开发卷，同请求携带的 pageApp+projectId 不生效。
    #[tokio::test]
    async fn service_context_wins_over_workspace_type() {
        let mut state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        // 开发卷根指到 tempdir，预建 app-5 目录（git 定位要求工作区已存在）
        std::fs::create_dir_all(tmp.path().join("app-5")).expect("seed dev volume");
        state.config = Arc::new(crate::Config {
            userapp_workspace_dir: tmp.path().to_path_buf(),
            ..crate::Config::default()
        });
        let (path, log_id) = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            let service = GitServiceContext::merge(None, Some("app-5"), None)
                .expect("merge with body app_id");
            resolve_target(
                &state,
                "pageApp",
                Some(&ProjectContext {
                    project_id: "proj-1".into(),
                    tenant_id: None,
                    space_id: None,
                    isolation_type: None,
                }),
                computer_ctx("u1", "c1").as_ref(),
                &service,
            )
            .await
        })
        .await
        .expect("serviceContext branch");
        assert_eq!(path, tmp.path().join("app-5"));
        assert_eq!(log_id, "computer:u1:c1");
    }

    /// serviceContext 激活但缺 userId/cId → Validation 拒绝（TS 文案同构）。
    #[tokio::test]
    async fn service_context_requires_user_and_cid() {
        let state = make_state();
        let err = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            let service = GitServiceContext::merge(None, Some("app-5"), None)
                .expect("merge with body app_id");
            resolve_target(&state, "taskAgent", None, None, &service).await
        })
        .await
        .expect_err("missing userId/cId must fail");
        assert!(
            err.to_string()
                .contains("serviceContext mode requires userId and cId"),
            "{err:?}"
        );
    }

    /// serviceContext 定位到的目录不存在 → Resource 错（git 只面向已存在工作区）。
    #[tokio::test]
    async fn service_context_missing_workspace_is_resource_error() {
        let state = make_state();
        // 默认 userapp 卷根 /app（测试环境不存在）
        let err = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            let service = GitServiceContext::merge(None, Some("app-nope"), None)
                .expect("merge with body app_id");
            resolve_target(
                &state,
                "taskAgent",
                None,
                computer_ctx("u1", "c1").as_ref(),
                &service,
            )
            .await
        })
        .await
        .expect_err("missing workspace must be resource error");
        assert!(
            matches!(err, AppError::Resource(..)),
            "应为资源不存在错误: {err:?}"
        );
        assert!(err.to_string().contains("Workspace does not exist"));
    }

    /// 构造失败回落：header userapp 但 appId 两通道皆缺（TS resolveServiceContext
    /// throw → extractServiceContext catch 置 null）→ workspaceType 老规则生效。
    #[tokio::test]
    async fn incomplete_service_context_falls_back_to_workspace_type() {
        let state = make_state();
        let (path, log_id) = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            // merge 读 header 通道（task-local）：userapp + appId 两通道皆缺 → 构造失败
            let service = GitServiceContext::merge(None, None, None);
            assert!(
                service.is_none(),
                "userapp without app_id must fail to merge"
            );
            // 回落后（unwrap_or_default 全 None → 不激活）workspaceType=taskAgent 生效
            let service = service.unwrap_or_default();
            assert!(!service.active());
            resolve_target(
                &state,
                "taskAgent",
                None,
                computer_ctx("u1", "c1").as_ref(),
                &service,
            )
            .await
        })
        .await
        .expect("fallback to workspaceType");
        let expected = RESOLVER_ROOT.with(|root| root.join("u1").join("c1"));
        assert_eq!(
            path, expected,
            "老规则 resolver 定位（userapp 标记被回落忽略）"
        );
        assert_eq!(log_id, "computer:u1:c1");
    }

    /// body/query 通道：无 header 时 serviceType/appId 走 body 字段归一
    /// （normalProject → {CWS}/{userId}/NormalProject/{projectId}）。
    #[tokio::test]
    async fn body_channel_normal_project_without_header() {
        let mut state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        state.config = Arc::new(crate::Config {
            computer_workspace_dir: tmp.path().to_path_buf(),
            ..crate::Config::default()
        });
        std::fs::create_dir_all(tmp.path().join("u1").join("NormalProject").join("proj-3"))
            .expect("seed workspace");
        let service = GitServiceContext::merge(Some("normalProject"), Some("proj-3"), None)
            .expect("body channel merge");
        let (path, _) = scope_context(None, None, async {
            resolve_target(
                &state,
                "taskAgent",
                None,
                computer_ctx("u1", "c1").as_ref(),
                &service,
            )
            .await
        })
        .await
        .expect("body channel normalProject");
        assert_eq!(
            path,
            tmp.path().join("u1").join("NormalProject").join("proj-3")
        );
    }

    /// workspacePath 通道激活 serviceContext：绑定目录直接定位（存在性检查通过）。
    #[tokio::test]
    async fn workspace_path_alone_activates_service_context() {
        let state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        let bound = tmp.path().join("ws");
        std::fs::create_dir_all(&bound).expect("seed bound workspace");
        let bound_str = bound.display().to_string();
        let service = GitServiceContext::merge(Some("taskAgent"), None, Some(bound_str.as_str()))
            .expect("workspace_path merge");
        assert!(service.active(), "workspacePath alone must activate");
        let (path, _) = scope_context(None, None, async {
            resolve_target(
                &state,
                "taskAgent",
                None,
                computer_ctx("u1", "c1").as_ref(),
                &service,
            )
            .await
        })
        .await
        .expect("workspacePath branch");
        assert_eq!(path, bound);
    }

    /// 空白值不激活（TS `String(x).trim() || null` 同构锁）：body workspacePath
    /// 为纯空白时不进入 serviceContext 分支，回落 workspaceType 老规则——
    /// 否则「假激活」会在 pageApp 场景错报 requires userId and cId（TS 侧 200）。
    #[tokio::test]
    async fn blank_body_values_do_not_activate_service_context() {
        let service = GitServiceContext::merge(Some("taskAgent"), Some("   "), Some("   "));
        let service = service.expect("taskAgent context merges (no appId requirement)");
        assert!(
            !service.active(),
            "blank workspacePath/appId must be filtered to None (not activate)"
        );
        // 空白 appId 在 userapp 语境 = 未携带 → 构造失败回落老规则
        let service = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            GitServiceContext::merge(None, Some("   "), None)
        })
        .await;
        assert!(
            service.is_none(),
            "userapp with blank appId must fail to merge"
        );
    }
}

//! `/api/git` HTTP handlers (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。
//!
//! 拆分: [`read`] (branches / tags / log / file-content / status) / [`write`]
//! (init / add / commit / unstage / discard / diff / reset / checkout / revert) /
//! [`refs`] (branch-create / branch-delete / branch-switch / tag-create / tag-delete)。
//! 本 mod.rs 提供共享定位收口（对齐 TS a29cbc0/1.4.7 `resolveAndCheck`：归一
//! 类型分派，pageApp 项目隔离模型优先——serviceContext 定位参数不再能覆盖）；
//! 共享 base 结构 (GitQuery / GitWriteBody) 定义在 [`crate::models`]。

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

/// git 族定位收口（对齐 TS a29cbc0/1.4.7 `resolveAndCheck`）：
///
/// 1. 归一类型：[`crate::extract::merged_workspace_kind`]（header
///    `x-workspace-type` > body/query `workspaceType`；垃圾值 400；完全未传
///    → 缺省 taskAgent 语义——v1.4.7 起 workspaceType 为可选参数）
/// 2. `pageApp`：项目隔离模型（projectId 必填 → `resolver.resolve_project`）。
///    **serviceContext 定位参数（appId/workspacePath）不再能覆盖 pageApp**——
///    上游 v1.4.6 的 bug：pageApp 请求携带 appId/workspacePath 被错落到会话
///    工作区执行破坏性 git 操作
/// 3. `userApp`/`normalProject`/`taskAgent`/缺省：会话工作区——userId/cId 必填；
///    userApp/normalProject 无 workspacePath 时必须 appId(projectId)（校验
///    前置于定位收口，文案对齐 TS）；定位复用 computer 域
///    [`crate::handlers::computer::computer_root_for_context`]（workspacePath
///    绑定 > userapp 开发卷 > normalProject 共享区 > 默认 resolver）
/// 4. 目标目录不存在 → `Resource` 错（"Workspace does not exist"，TS 同款——
///    git 操作只面向已存在工作区，不创建）
///
/// 有意偏离：header 为 userApp/normalProject 且缺 appId 时，TS 的
/// `resolveServiceContext` throw（缺 appId）被 `extractServiceContext` catch
/// 置 null，类型回退 body 显式 workspaceType——body 为 taskAgent/pageApp 时
/// 按回退值定位甚至 200（静默错定位的残留形态）；此处按 header 归一类型
/// fail-fast 400 "appId(projectId) is required…"（方向更严），不复刻
/// throw-catch-null 回退链。
async fn resolve_target(
    state: &AppState,
    body_workspace_type: Option<&str>,
    body_app_id: Option<&str>,
    body_workspace_path: Option<&str>,
    project_ctx: Option<&ProjectContext>,
    computer_ctx: Option<&ComputerContext>,
) -> Result<(PathBuf, String), AppError> {
    let kind = crate::extract::merged_workspace_kind(body_workspace_type, None)?;
    if matches!(kind, Some(ComputerServiceKind::PageApp)) {
        let ctx =
            project_ctx.ok_or_else(|| AppError::validation("pageApp mode requires projectId"))?;
        let path = git::resolve_page_project(&*state.resolver, ctx).await?;
        return Ok((path, ctx.project_id.clone()));
    }
    let ctx = computer_ctx
        .filter(|ctx| !ctx.user_id.trim().is_empty() && !ctx.cid.trim().is_empty())
        .ok_or_else(|| {
            AppError::validation("conversation workspace mode requires userId and cId")
        })?;
    let workspace_path = crate::extract::merged_workspace_path(body_workspace_path);
    let app_id = crate::extract::merged_request_app_id(body_app_id);
    if let Some(type_name) = match kind {
        Some(ComputerServiceKind::Userapp) => Some("userApp"),
        Some(ComputerServiceKind::NormalProject) => Some("normalProject"),
        _ => None,
    } && workspace_path.is_none()
        && app_id.is_none()
    {
        return Err(AppError::validation(format!(
            "appId(projectId) is required for {type_name} workspace when workspacePath is absent"
        )));
    }
    let path = crate::handlers::computer::computer_root_for_context(
        state,
        &ctx.user_id,
        &ctx.cid,
        kind,
        app_id.as_deref(),
        workspace_path.as_deref(),
    )
    .await?;
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("Workspace does not exist"));
    }
    // logId 与 TS 会话分支同形态（`computer:{userId}:{cId}`）
    Ok((path, format!("computer:{}:{}", ctx.user_id, ctx.cid)))
}

/// GET 路由解析: GitQuery → (workspace path, logId)。
pub(super) async fn resolve(q: &GitQuery, state: &AppState) -> Result<(PathBuf, String), AppError> {
    resolve_target(
        state,
        q.workspace_type.as_deref(),
        q.app_id.as_deref(),
        q.workspace_path.as_deref(),
        project_ctx(q).as_ref(),
        computer_ctx(q).as_ref(),
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
    resolve_target(
        state,
        body.workspace_type.as_deref(),
        body.app_id.as_deref(),
        body.workspace_path.as_deref(),
        project_ctx.as_ref(),
        computer_ctx.as_ref(),
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
                preview: None,
                started_at: std::time::Instant::now(),
            }
        })
    }

    /// task-local header 通道 scope 助手（模拟中间件注入；WORKSPACE_TYPE_RAW
    /// 留空 = header 通道无原始值，垃圾 header 用例另行显式 scope）。
    async fn scope_context<F, T>(kind: Option<ComputerServiceKind>, app_id: Option<&str>, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let app_id = app_id.map(str::to_string);
        crate::extract::WORKSPACE_KIND
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

    fn proj_ctx(id: &str) -> Option<ProjectContext> {
        Some(ProjectContext {
            project_id: id.into(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        })
    }

    /// 1.4.7 优先级反转锁：归一类型=pageApp 时项目模型优先——serviceContext
    /// 定位参数（header x-app-id / body appId）不再能把 pageApp 请求错落到
    /// 会话工作区（上游 v1.4.6 修复的核心 bug 形态）。
    #[tokio::test]
    async fn page_app_project_model_wins_over_service_context() {
        let state = make_state();
        std::fs::create_dir_all(RESOLVER_ROOT.with(|root| root.join("project").join("proj-1")))
            .expect("seed project dir");
        let (path, log_id) =
            scope_context(Some(ComputerServiceKind::PageApp), Some("app-5"), async {
                resolve_target(
                    &state,
                    None,
                    // body appId 亦在场——不得改变 pageApp 分派
                    Some("app-5"),
                    None,
                    proj_ctx("proj-1").as_ref(),
                    computer_ctx("u1", "c1").as_ref(),
                )
                .await
            })
            .await
            .expect("pageApp project model");
        assert_eq!(
            path,
            RESOLVER_ROOT.with(|root| root.join("project").join("proj-1"))
        );
        assert_eq!(log_id, "proj-1");
    }

    /// pageApp 缺 projectId → 400（项目模型必填，serviceContext 参数不能替代）。
    #[tokio::test]
    async fn page_app_requires_project_id() {
        let state = make_state();
        let err = scope_context(Some(ComputerServiceKind::PageApp), Some("app-5"), async {
            resolve_target(
                &state,
                None,
                Some("app-5"),
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect_err("pageApp without projectId must fail");
        assert!(
            err.to_string().contains("pageApp mode requires projectId"),
            "{err:?}"
        );
    }

    /// userApp 会话分支：header userapp + appId 落开发卷（1.4.7 四值词表内
    /// 合法类型；pageApp 参数同请求在场不得干扰）。
    #[tokio::test]
    async fn userapp_with_app_id_resolves_dev_volume() {
        let mut state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("app-5")).expect("seed dev volume");
        state.config = Arc::new(crate::Config {
            userapp_workspace_dir: tmp.path().to_path_buf(),
            ..crate::Config::default()
        });
        let (path, log_id) = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            resolve_target(
                &state,
                Some("userApp"),
                Some("app-5"),
                None,
                proj_ctx("proj-1").as_ref(),
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect("userApp session branch");
        assert_eq!(path, tmp.path().join("app-5"));
        assert_eq!(log_id, "computer:u1:c1");
    }

    /// normalProject + appId（projectId 复用 appId 通道）→ 共享工作区
    /// `{CWS}/{userId}/NormalProject/{projectId}`（body 通道类型，无 header）。
    #[tokio::test]
    async fn normal_project_resolves_shared_workspace() {
        let mut state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("u1").join("NormalProject").join("proj-3"))
            .expect("seed workspace");
        state.config = Arc::new(crate::Config {
            computer_workspace_dir: tmp.path().to_path_buf(),
            ..crate::Config::default()
        });
        let (path, log_id) = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("normalProject"),
                Some("proj-3"),
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect("normalProject shared workspace");
        assert_eq!(
            path,
            tmp.path().join("u1").join("NormalProject").join("proj-3")
        );
        assert_eq!(log_id, "computer:u1:c1");
    }

    /// 会话分支缺 userId/cId → 400（文案对齐 TS v1.4.7
    /// "conversation workspace mode requires userId and cId"）。
    #[tokio::test]
    async fn conversation_workspace_requires_user_and_cid() {
        let state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        let bound = tmp.path().join("ws");
        std::fs::create_dir_all(&bound).expect("seed bound workspace");
        let bound_str = bound.display().to_string();
        let err = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("taskAgent"),
                None,
                Some(bound_str.as_str()),
                None,
                None,
            )
            .await
        })
        .await
        .expect_err("missing userId/cId must fail");
        assert!(
            err.to_string()
                .contains("conversation workspace mode requires userId and cId"),
            "{err:?}"
        );
    }

    /// userApp/normalProject 无 workspacePath 且无 appId → 400（v1.4.7：不再
    /// 静默回落老规则；空白 appId 视为未传，同 400）。
    #[tokio::test]
    async fn userapp_or_normal_project_without_app_id_rejected() {
        let state = make_state();
        let err = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            resolve_target(
                &state,
                Some("userApp"),
                None,
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect_err("userApp without appId must fail fast");
        assert!(
            err.to_string().contains(
                "appId(projectId) is required for userApp workspace when workspacePath is absent"
            ),
            "{err:?}"
        );

        // normalProject 同规（body 通道类型，无 header）
        let err = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("normalProject"),
                None,
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect_err("normalProject without appId must fail fast");
        assert!(
            err.to_string().contains(
                "appId(projectId) is required for normalProject workspace when workspacePath is absent"
            ),
            "{err:?}"
        );

        // 空白 body appId 视为未传（trim 过滤），同 400
        let err = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("userApp"),
                Some("   "),
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect_err("blank appId treated as missing");
        assert!(
            err.to_string()
                .contains("appId(projectId) is required for userApp"),
            "{err:?}"
        );
    }

    /// 定位目录不存在 → Resource 错（git 只面向已存在工作区，不创建）。
    #[tokio::test]
    async fn missing_workspace_is_resource_error() {
        let state = make_state();
        // 默认 userapp 卷根（测试环境不存在）→ userApp+appId 定位必然落空
        let err = scope_context(Some(ComputerServiceKind::Userapp), None, async {
            resolve_target(
                &state,
                Some("userApp"),
                Some("app-nope"),
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
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

    /// workspacePath 绑定（会话分支定位最高优先）：body 通道与 header-only
    /// 形态都直接落绑定目录（header-only 为 P1 回归锁——header 即绑定）。
    #[tokio::test]
    async fn workspace_path_binding_body_and_header_channels() {
        let state = make_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        let bound = tmp.path().join("ws");
        std::fs::create_dir_all(&bound).expect("seed bound workspace");
        let bound_str = bound.display().to_string();

        // body 通道
        let (path, _) = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("taskAgent"),
                None,
                Some(bound_str.as_str()),
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect("body workspacePath binding");
        assert_eq!(path, bound);

        // header-only（仅 x-workspace-path header）
        let (path, _) = crate::extract::WORKSPACE_PATH
            .scope(Some(bound_str), async {
                resolve_target(
                    &state,
                    Some("taskAgent"),
                    None,
                    None,
                    None,
                    computer_ctx("u1", "c1").as_ref(),
                )
                .await
            })
            .await
            .expect("header-only workspacePath binding");
        assert_eq!(path, bound);
    }

    /// v1.4.7 放宽锁：workspaceType 完全未传（TS 侧本就可选）+ userId/cId →
    /// 缺省 taskAgent 会话布局成功定位（v1.4.6/旧移植版此形态 400）。
    #[tokio::test]
    async fn absent_workspace_type_defaults_to_conversation_workspace() {
        let state = make_state();
        let (path, log_id) = scope_context(None, None, async {
            resolve_target(
                &state,
                None,
                None,
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect("default conversation workspace");
        assert_eq!(path, RESOLVER_ROOT.with(|root| root.join("u1").join("c1")));
        assert_eq!(log_id, "computer:u1:c1");
    }

    /// 垃圾 workspaceType → 400（git 域 fail-fast，与 extract/computer 域
    /// 同一收口文案）：body 通道垃圾、header 通道垃圾均拒绝。
    #[tokio::test]
    async fn garbage_workspace_type_rejected() {
        let state = make_state();
        let err = scope_context(None, None, async {
            resolve_target(
                &state,
                Some("not-a-type"),
                None,
                None,
                None,
                computer_ctx("u1", "c1").as_ref(),
            )
            .await
        })
        .await
        .expect_err("garbage body workspaceType must fail");
        assert!(
            err.to_string().contains(
                "workspaceType must be one of userApp, pageApp, normalProject, taskAgent"
            ),
            "{err:?}"
        );

        // header 通道垃圾（raw 有值、归一 None）
        let err = crate::extract::WORKSPACE_TYPE_RAW
            .scope(
                Some("junk".into()),
                crate::extract::WORKSPACE_KIND.scope(None, async {
                    resolve_target(
                        &state,
                        None,
                        None,
                        None,
                        None,
                        computer_ctx("u1", "c1").as_ref(),
                    )
                    .await
                }),
            )
            .await
            .expect_err("garbage header workspaceType must fail");
        assert!(
            err.to_string()
                .contains("workspaceType must be one of userApp"),
            "{err:?}"
        );
    }
}

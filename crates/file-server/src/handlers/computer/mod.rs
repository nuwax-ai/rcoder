//! `/api/computer` HTTP handlers (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`
//! (或项目绑定目录 workspaceDir, 对齐 TS f979df7)。
//!
//! 拆分: [`files_read`] (get-file-list / resolve-file / search-files) /
//! [`files`] (files-update / upload / generate-file / import-project / delete-workspace) /
//! [`archive`] (zip-workspace / download-all-files) / [`workspace`] (create-workspace /
//! push-skills / init-project-template) / [`exec`] (execute-command / get-logs) /
//! [`packages`] (install-project / build-agent-package / cleanup-build-artifacts)。
//! 本 mod.rs 仅提供跨组共享 helper。

use std::path::{Path, PathBuf};

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ComputerContext;

// ID 字段反序列化 helper (deserialize_id_string / deserialize_optional_id_string) 已提升至
// `crate::extract`, 供 computer / project 等所有 handler 共用。

pub mod archive;
pub mod exec;
pub mod files;
pub mod files_read;
pub mod fs_browser;
pub mod packages;
pub mod workspace;

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

async fn ws_path(
    state: &AppState,
    user_id: &str,
    cid: &str,
    workspace_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    computer_root_for_request(state, user_id, cid, workspace_dir).await
}

/// computer 域请求根目录（userApp 分流 + 绑定目录的**单一收口**——ws_path 与
/// 静态文件共用，消除多处独立 if 的漂移面）。
///
/// 定位优先级（对齐 TS f979df7 `resolveWorkspaceDir`）：
/// 1. **项目绑定目录 workspaceDir**（header `x-workspace-dir` > body/query 显式值，
///    经 [`crate::extract::merged_workspace_dir`] 合并）→ [`crate::workspace::normalize_workspace_dir`]
///    fail-fast 校验后直接使用。短路在 userapp 分流与 resolver **之前**——不经
///    Subvolume resolver 的 ensure-PVC 副作用（携带非法绑定目录的请求不得创建
///    PVC）。绑定目录信任模型与 customTargetDir 一致（不做根白名单，容器/沙箱
///    内网部署）。
///    ⚠️ single-app 模式（生产运行容器）fail-closed 拒绝绑定——运行容器只服务
///    本 app 卷，与 customTargetDir 在该模式的收紧先例一致（workspace.rs
///    `resolve_userapp_dev`）；TS 无 single-app 概念，此为有意偏离。
/// 2. userApp 分流（X-Service-Type=userapp，经反向代理/rcoder 拦截层透传）：
///    workspace 从 computer 定位 `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}` 切到
///    开发卷 `{USERAPP_WORKSPACE_DIR}/{cId}`（cId=app_id；本容器即该 app 的开发容器）。
/// 3. 默认 resolver（Local `{root}/{userId}/{cId}` / Subvolume per-agent PVC）。
pub(crate) async fn computer_root_for_request(
    state: &AppState,
    user_id: &str,
    cid: &str,
    workspace_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    if let Some(raw) = crate::extract::merged_workspace_dir(workspace_dir) {
        let single_app = state
            .config
            .userapp_single_app_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if single_app {
            return Err(AppError::validation(
                "workspaceDir is not allowed in single-app mode \
                 (this container serves its own app volume only)",
            ));
        }
        let dir = crate::workspace::normalize_workspace_dir(&raw)?;
        return Ok(PathBuf::from(dir));
    }
    if crate::extract::is_userapp_request() {
        return crate::workspace::resolve_userapp_dev(cid, None, &state.config);
    }
    state
        .resolver
        .resolve_computer(&ComputerContext {
            user_id: user_id.to_string(),
            cid: cid.to_string(),
        })
        .await
}

/// computer 目标路径: `customTargetDir` trim 后非空则用之, 否则回退默认工作区 (对齐 nuwax)。
///
/// 注: `customTargetDir` 完全信任调用方, **不做根目录白名单限制**:
/// 产品运行于容器内、内网私有化部署, 且用户客户端复用本 file-server 模块逻辑,
/// 每个用户电脑上的路径各不相同, 限制根路径会误伤正常业务;
/// 其内部相对路径仍由 [`crate::path_safety::ensure_within`] 防逃逸。
///
/// 优先级（对齐 TS f979df7 targetDir 链）: customTargetDir > workspaceDir > 默认。
async fn resolve_computer_target(
    state: &AppState,
    user_id: &str,
    cid: &str,
    custom_target_dir: Option<&str>,
    workspace_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    let default_path = ws_path(state, user_id, cid, workspace_dir).await?;
    match custom_target_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ct) => Ok(PathBuf::from(ct)),
        None => Ok(default_path),
    }
}

/// agent-store 用户级根目录（create-workspace-v2 / push-skills 实体存储的锚定点，
/// 对齐 TS f979df7 `agentStoreUtils.getAgentStorePath`——store **始终锚定配置根**，
/// 不随会话绑定目录漂移，防 `.agent-store` 写进绑定目录的任意父目录）：
///
/// - userapp 分流 → `{USERAPP_WORKSPACE_DIR}`（开发卷自身，无 userId 段）
/// - 项目绑定目录（非 userapp）→ `{COMPUTER_WORKSPACE_DIR}/{userId}`
/// - 默认 → `ws.parent()`（Local=`{root}/{userId}` 与 TS 等价；
///   Subvolume=per-user PVC 稳定根，既有阶段 2 语义不动）
///
/// `session_workspace` 与 `user_root` 解耦的原因：默认布局下两者同树
/// （ws = user_root/cid），绑定布局下会话工作区=绑定目录、store=配置根，
/// 不能再从 `user_root.join(cid)` 倒推会话目录（见
/// `computer_ws::create_workspace_with_agent_store` 的显式 session_workspace 参数）。
pub(crate) fn agent_store_user_root(
    state: &AppState,
    user_id: &str,
    ws: &Path,
    workspace_dir: Option<&str>,
) -> PathBuf {
    if crate::extract::is_userapp_request() {
        return state.config.userapp_workspace_dir.clone();
    }
    if crate::extract::merged_workspace_dir(workspace_dir).is_some() {
        return state.config.computer_workspace_dir.join(user_id);
    }
    ws.parent().unwrap_or(ws).to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::error::AppResult;
    use crate::workspace::{ComputerContext, ProjectContext, WorkspaceResolver};

    /// 计数 mock resolver: 记录 resolve_computer 调用次数——
    /// 断言"绑定时 resolver 零调用"(防 Subvolume ensure-PVC 副作用回归的核心探针)。
    struct CountingResolver {
        root: PathBuf,
        computer_calls: AtomicUsize,
    }

    #[async_trait]
    impl WorkspaceResolver for CountingResolver {
        async fn resolve_project(&self, _ctx: &ProjectContext) -> AppResult<PathBuf> {
            Err(AppError::system("computer 域测试不触达 resolve_project"))
        }

        async fn resolve_computer(&self, ctx: &ComputerContext) -> AppResult<PathBuf> {
            self.computer_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.root.join(&ctx.user_id).join(&ctx.cid))
        }
    }

    fn make_state() -> (AppState, Arc<CountingResolver>) {
        let config = Arc::new(crate::Config::default());
        let resolver = Arc::new(CountingResolver {
            root: config.computer_workspace_dir.clone(),
            computer_calls: AtomicUsize::new(0),
        });
        let state = AppState {
            resolver: resolver.clone(),
            dev_server: Arc::new(crate::DevServerManager::new(config.clone())),
            build_manager: Arc::new(crate::BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(crate::LogCacheManager::new(&config)),
            skill_downloader: Arc::new(
                crate::SkillDownloader::new(&config).expect("construct skill downloader"),
            ),
            config,
            started_at: std::time::Instant::now(),
        };
        (state, resolver)
    }

    #[tokio::test]
    async fn no_binding_resolves_default_two_level() {
        let (state, resolver) = make_state();
        let path = computer_root_for_request(&state, "u1", "c1", None)
            .await
            .expect("resolve default");
        // 默认二级布局 {computer_root}/{user}/{cid} (现状回归)
        assert_eq!(
            path,
            state.config.computer_workspace_dir.join("u1").join("c1")
        );
        assert_eq!(resolver.computer_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bound_dir_short_circuits_resolver_and_normalizes() {
        let (state, resolver) = make_state();
        // 反斜杠输入 → 归一为 / (对齐 TS canonicalizeDir)
        let path = computer_root_for_request(&state, "u1", "c1", Some("/tmp/bound\\\\ws"))
            .await
            .expect("resolve bound");
        assert_eq!(path, PathBuf::from("/tmp/bound/ws"));
        // 绑定短路在 resolver 之前: 零调用 (Subvolume ensure-PVC 副作用不可达)
        assert_eq!(resolver.computer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bound_dir_wins_over_userapp_flag() {
        let (state, resolver) = make_state();
        let path = crate::extract::USERAPP_FLAG
            .scope(true, async {
                computer_root_for_request(&state, "u1", "c1", Some("/tmp/bound")).await
            })
            .await
            .expect("bound wins over userapp");
        assert_eq!(path, PathBuf::from("/tmp/bound"));
        assert_eq!(resolver.computer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn userapp_flag_without_binding_keeps_dev_volume() {
        let (state, _resolver) = make_state();
        let path = crate::extract::USERAPP_FLAG
            .scope(true, async {
                computer_root_for_request(&state, "u1", "app-9", None).await
            })
            .await
            .expect("userapp dev volume");
        // 现状回归: 无绑定时 userapp 分流 → {USERAPP_WORKSPACE_DIR}/{cId}
        assert_eq!(path, state.config.userapp_workspace_dir.join("app-9"));
    }

    #[tokio::test]
    async fn invalid_bound_dirs_fail_fast_400() {
        let (state, resolver) = make_state();
        for bad in ["relative/path", "/a/../b", "C:", "/a/./b"] {
            let err = computer_root_for_request(&state, "u1", "c1", Some(bad))
                .await
                .expect_err(bad);
            assert!(matches!(err, AppError::Validation(..)), "{bad}: {err:?}");
        }
        // 无效绑定同样不触达 resolver
        assert_eq!(resolver.computer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn single_app_mode_rejects_bound_dir() {
        let (mut state, _resolver) = make_state();
        state.config = Arc::new(crate::Config {
            userapp_single_app_id: Some("app-owned".into()),
            ..crate::Config::default()
        });
        let err = computer_root_for_request(&state, "u1", "c1", Some("/tmp/bound"))
            .await
            .expect_err("single-app must reject binding");
        assert!(err.to_string().contains("single-app"));
    }

    #[tokio::test]
    async fn custom_target_dir_wins_over_bound_dir() {
        // 优先级: customTargetDir > workspaceDir > 默认 (对齐 TS targetDir 链)
        let (state, resolver) = make_state();
        let path = resolve_computer_target(
            &state,
            "u1",
            "c1",
            Some("/tmp/custom-target"),
            Some("/tmp/bound"),
        )
        .await
        .expect("custom wins");
        assert_eq!(path, PathBuf::from("/tmp/custom-target"));
        assert_eq!(resolver.computer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn agent_store_user_root_anchors_per_layout() {
        let (state, _resolver) = make_state();

        // 默认布局: ws.parent() (Local={root}/{userId} 与 TS 等价)
        let ws = state.config.computer_workspace_dir.join("u1").join("c1");
        assert_eq!(
            agent_store_user_root(&state, "u1", &ws, None),
            state.config.computer_workspace_dir.join("u1")
        );

        // 绑定布局: store 锚定配置根 {COMPUTER_WORKSPACE_DIR}/{userId}, 不随绑定漂移
        assert_eq!(
            agent_store_user_root(&state, "u1", Path::new("/tmp/bound"), Some("/tmp/bound")),
            state.config.computer_workspace_dir.join("u1")
        );

        // userapp 分流: 开发卷自身 (无 userId 段)
        let root = crate::extract::USERAPP_FLAG
            .scope(true, async {
                agent_store_user_root(&state, "u1", Path::new("/any/ws"), None)
            })
            .await;
        assert_eq!(root, state.config.userapp_workspace_dir);
    }
}

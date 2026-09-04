//! userApp 文件域转发层（rcoder 侧编排，实际处理在 per-app 开发容器内 file-server）。
//!
//! - `forward`：`/api/v1/userapp` 显式透传清单（见
//!   `CONTAINER_PASS_THROUGH_PATHS`），并承接 `/api/computer/*` 拦截层
//!   （`X-Service-Type: userapp` 分流时反向代理转来的 TS 老路径原样透传）
//! - `workspace`：`POST /api/v1/userapp/workspace` 创建项目显式入口
//! - `db`：`POST /api/v1/userapp/db/{app_stage}/reset-password|create-database`
//!   PG 账号/库管理（凭据对齐已内嵌 start 部署链，独立接口下线）
//! - 本模块：路由聚合 + 开发容器 ensure-workspace 公共调用
//!
//! 容器定位/创建复用 `crate::userapp_builder::ensure_userapp_builder`
//! （幂等；注册 state.projects 防孤立清理）。

pub(crate) mod db;
pub(crate) mod forward;
pub(crate) mod semantics;
pub(crate) mod upstream;
pub(crate) mod workspace;

use std::sync::Arc;

use axum::routing::{Router, any, post};

use crate::router::AppState;

// 分流 header 常量（X-Service-Type / X-App-Id）定义在 shared_types（与容器内
// file-server 共用单一事实源）；本模块转发 computer_intercept 拦截层给主 Router
// 装配。chat body 的 service_type 词表由 shared_types::ChatServiceScope 枚举承载。
pub(crate) use forward::computer_intercept;
pub(crate) use upstream::invalidate_probe_cache;

/// 容器 file-server 对外接口的**显式透传清单**（全路径；handler 统一复用
/// [`forward::forward_userapp`]，method/path/query/body 原样流式转发，容器侧
/// 自答 405/404）。
///
/// 早先是单条 `{*rest}` catch-all 兜底：分派语义只在源码可见（OpenAPI 文档由
/// 注解静态聚合、路由真值却靠兜底）、未声明路径误差传到容器错误形态漂移，
/// 且与 app_manager 的 `{app_id}` 参数路由进入同一棵 matchit 树后结构性冲突
/// （同段 param 与 catch-all 不共存）启动即 panic——故逐条显式枚举。其中 12 条
/// TS 同名老路径族虽已从主文档剔除（分流代理内部面），仍可能有残留调用方经
/// rcoder 访问，照常登记保持可达。
///
/// 单一事实源供路由生成本处消费；与容器侧/主文档的一致性由守卫测试锁定
/// （`pass_through_paths_all_exist_in_container_doc` 防死链、
/// `primary_userapp_paths_are_fully_handled_by_route_tables` 双向闭包）——
/// 容器新增对外接口须同步登记此表，契约变更当场报红，不再依赖人肉对齐。
pub(crate) const CONTAINER_PASS_THROUGH_PATHS: &[&str] = &[
    // 构建链（builder 编排：构建/任务流转/项目探测确认）
    "/api/v1/userapp/build",
    "/api/v1/userapp/tasks/{task_id}",
    "/api/v1/userapp/tasks/{task_id}/logs/stream",
    "/api/v1/userapp/tasks/{task_id}/cancel",
    // projects/detect|confirm、install-project 已上收 `{app_id}/{app_stage}` 门面路由
    // （见 routes_for_env_flattened：dev-only，app_stage 折叠后仍以容器平铺契约转发）
    // 文件镜像（TS nuwax-file-server 同名老接口族）
    "/api/v1/userapp/get-file-list",
    "/api/v1/userapp/resolve-file",
    "/api/v1/userapp/search-files",
    "/api/v1/userapp/files-update",
    "/api/v1/userapp/upload-file",
    "/api/v1/userapp/upload-files",
    "/api/v1/userapp/generate-file",
    "/api/v1/userapp/import-project",
    // 容器文件操作（新形态 app-files 族）——对外经 `{app_id}/{app_stage}/upload 族`
    // 与 dev storage/clear 面（容器契约端点保留，对外平铺不再注册）
    // 开发工具链
    "/api/v1/userapp/ensure-workspace",
    "/api/v1/userapp/execute-command",
    "/api/v1/userapp/get-logs",
    "/api/v1/userapp/zip-workspace",
    "/api/v1/userapp/download-all-files",
    "/api/v1/userapp/init-project-template",
    "/api/v1/userapp/push-skills-to-workspace",
    // dev server 进程管理
    "/api/v1/userapp/dev/start",
    "/api/v1/userapp/dev/stop",
    "/api/v1/userapp/dev/restart",
    "/api/v1/userapp/dev/list",
    "/api/v1/userapp/dev/framework-info",
    // 静态资源（按 releaseId 取包；注解侧另挂 OPTIONS，any 已覆盖）
    "/api/v1/userapp/static/{app_id}",
];

/// rcoder 本地实现的 userapp 路径快照等守卫夹具（仅供测试；随测试编译，
/// 避免运行时 dead_code 告警）。
#[cfg(test)]
pub(crate) mod guard_tables {
    /// rcoder 本地实现的 userapp 路径快照（`routes()` 显式入口部分；
    /// 守卫闭包比对用——改动路由须同步）。
    pub(crate) const LOCAL_USERAPP_PATHS: [&str; 15] = [
        "/api/v1/userapp/workspace",
        "/api/v1/userapp/db/{app_stage}/reset-password",
        "/api/v1/userapp/db/{app_stage}/create-database",
        // `{app_stage}` 门面折叠路由（dev-only 构建链；URI 还原容器平铺契约转发）
        "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
        "/api/v1/userapp/{app_id}/{app_stage}/projects/confirm",
        "/api/v1/userapp/{app_id}/{app_stage}/install-project",
        // 307 代理文档族（userapp_proxy.rs 挂载；路径与 Pingora 真实路由同形态）
        "/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}/{*path}",
        "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}/{*path}",
    ];

    /// app_manager 具体路由路径快照（crates/app_manager/src/routes.rs；同样供
    /// 守卫闭包比对——该清单增删须同步。原 `{app_id}/db/*` 两路已下线，数据库
    /// 管理统一走转发层 `/api/v1/userapp/db/{app_stage}/*`；文件/存储八接口已加
    /// `{app_stage}` 段显式分派 dev/prod）。
    pub(crate) const APP_MANAGER_PATHS: [&str; 24] = [
        "/api/v1/userapp/query",
        "/api/v1/userapp/runtime",
        "/api/v1/userapp/{app_id}",
        "/api/v1/userapp/{app_id}/update",
        "/api/v1/userapp/{app_id}/{app_stage}/delete",
        "/api/v1/userapp/{app_id}/delete/app",
        "/api/v1/userapp/{app_id}/start",
        "/api/v1/userapp/{app_id}/stop",
        "/api/v1/userapp/{app_id}/restart",
        "/api/v1/userapp/{app_id}/{app_stage}/recycle-policy",
        "/api/v1/userapp/{app_id}/{app_stage}/logs/sources/query",
        "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
        "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
        "/api/v1/userapp/{app_id}/{app_stage}/health",
        "/api/v1/userapp/{app_id}/{app_stage}/stats",
        "/api/v1/userapp/{app_id}/{app_stage}/events",
        "/api/v1/userapp/{app_id}/{app_stage}/upload",
        "/api/v1/userapp/{app_id}/{app_stage}/upload-from-url",
        "/api/v1/userapp/{app_id}/{app_stage}/files",
        "/api/v1/userapp/{app_id}/{app_stage}/files/delete",
        "/api/v1/userapp/{app_id}/{app_stage}/storage",
        "/api/v1/userapp/{app_id}/{app_stage}/storage/clear",
        "/api/v1/userapp/{app_id}/{app_stage}/storage/destroy",
        "/api/v1/userapp/storage/{app_stage}/query",
    ];
}

/// userApp 域路由（挂 rcoder 主 Router）：本地实现入口 + 显式透传清单
/// ＋ `{app_stage}` 门面折叠路由。
///
/// `/api/v1/userapp` 族不再来自 file-server 本地路由——聚合文档时已剔除。
pub fn routes() -> Router<Arc<AppState>> {
    let mut router = Router::new()
        // `{app_stage}` 门面（dev-only 构建链；URI 折叠回容器平铺契约后定向转发：
        // detect/confirm=项目类型探测确认、install-project=模板安装）
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
            post(forward::flat_dev_projects_detect),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/projects/confirm",
            post(forward::flat_dev_projects_confirm),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/install-project",
            post(forward::flat_dev_install_project),
        )
        .route(
            "/api/v1/userapp/workspace",
            post(workspace::create_workspace),
        )
        .route(
            "/api/v1/userapp/db/{app_stage}/reset-password",
            post(db::reset_password),
        )
        .route(
            "/api/v1/userapp/db/{app_stage}/create-database",
            post(db::create_database),
        );
    for path in CONTAINER_PASS_THROUGH_PATHS {
        router = router.route(path, any(forward::forward_userapp));
    }
    router
}

/// 容器内 file-server 幂等建 workspace 目录（execute-command 等接口的 cwd 前置；
/// create-workspace / chat 开发对话 / db 对齐共用的公共调用）。
///
/// 全新容器的 file-server 有启动窗口（镜像全套 agent_runner+PG+file-server），
/// 连接类失败按 5s/10s/15s 退避重试（HTTP 4xx/5xx 业务错误不重试，直接上抛）。
/// 错误返回面向日志的描述串（调用方各自映射响应类型）。
pub(crate) async fn ensure_workspace_via_dev(
    addr: &str,
    app_id: &str,
    user_id: &str,
) -> Result<(), String> {
    // 五档退避最坏 120s：agent_runner(file-server 60000) 在宿主高负载（多 builder 并发
    // 构建/对话）下启动可超 30s——原三档 30s 上限在 e2e 六场景并行时实测不够
    // （后发容器被先发容器负载拖慢 → 60000 连接失败）。
    // 总预算 150s 封顶：无封顶时五档 sleep(120s)+六次 timeout(最坏 180s)≈300s，
    // 而上游客户端（浏览器/网关 60-120s）必然先超时，服务端剩余退避全部白跑、
    // 还占用 axum 连接与 tokio task。
    const BACKOFF_SECS: [u64; 5] = [5, 10, 15, 30, 60];
    const TOTAL_BUDGET_SECS: u64 = 150;
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(TOTAL_BUDGET_SECS);
    let mut last_err = String::new();
    for (attempt, delay) in std::iter::once(0u64)
        .chain(BACKOFF_SECS.iter().copied())
        .enumerate()
    {
        if attempt > 0 {
            let sleep = std::time::Duration::from_secs(delay);
            if tokio::time::Instant::now() + sleep >= deadline {
                tracing::warn!(
                    "[USERAPP_FORWARD] ensure-workspace budget {TOTAL_BUDGET_SECS}s exhausted, stop retrying: app_id={app_id}"
                );
                break;
            }
            tracing::info!(
                "[USERAPP_FORWARD] ensure-workspace retry {}/5 after {delay}s (dev container starting): app_id={app_id}",
                attempt
            );
            tokio::time::sleep(sleep).await;
        }
        let resp = crate::http_client::shared_client()
            .post(format!("{addr}/api/v1/userapp/ensure-workspace"))
            .timeout(std::time::Duration::from_secs(30))
            .json(&serde_json::json!({"app_id": app_id, "user_id": user_id}))
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                // 业务错误（4xx/5xx 响应）重试无益，直接上抛
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("ensure-workspace returned {status}: {text}"));
            }
            Err(e) => {
                last_err = format!("dev container ensure-workspace failed: {e}");
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::workspace::CreateWorkspaceBody;
    use super::*;

    #[test]
    fn create_workspace_body_is_snake_case() {
        let raw = serde_json::json!({"app_id": "app-1", "user_id": "u1"});
        let body: CreateWorkspaceBody = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(body.app_id, "app-1");
        assert_eq!(body.user_id, "u1");
        // 旧 camel wire 已废弃：未知键被忽略后必填字段缺失即拒
        let legacy = serde_json::json!({"appId": "app-1", "userId": "u1"});
        assert!(serde_json::from_value::<CreateWorkspaceBody>(legacy).is_err());
    }

    #[test]
    fn container_pass_through_paths_are_unique() {
        let unique: std::collections::HashSet<_> = CONTAINER_PASS_THROUGH_PATHS.iter().collect();
        assert_eq!(
            unique.len(),
            CONTAINER_PASS_THROUGH_PATHS.len(),
            "duplicate pass-through path registered"
        );
    }

    /// 启动 panic 回归锚：userapp 域全部 pattern 平铺注册必须成功。此前
    /// `{*rest}` catch-all 与 `{app_id}` 参数段同树时 matchit 直接 panic，
    /// 且该炸点只有起服务才暴露（单测不构建整棵 router）——以同等 pattern
    /// 集合在此固化为快门；今后再往本域引入 catch-all 形态路径即红。
    #[test]
    fn merged_userapp_route_table_builds_without_matchit_conflict() {
        async fn ok() -> &'static str {
            "ok"
        }
        let mut table = Router::<()>::new();
        for path in guard_tables::LOCAL_USERAPP_PATHS
            .iter()
            .chain(guard_tables::APP_MANAGER_PATHS.iter())
            .chain(CONTAINER_PASS_THROUGH_PATHS.iter())
        {
            table = table.route(path, any(ok));
        }
        // 构建即完成 matchit 插入校验，结果无需保留
        drop(table);
    }

    /// 防死链：每条透传路由必须仍存在于容器侧对外文档——容器已删除接口仍在
    /// 此登记的话，请求会一路 404 到容器才暴露，链路不可见。
    #[test]
    fn pass_through_paths_all_exist_in_container_doc() {
        let document = file_server_userapp::document();
        for path in CONTAINER_PASS_THROUGH_PATHS {
            assert!(
                document.paths.paths.contains_key(*path),
                "pass-through path missing in container doc: {path}"
            );
        }
    }
}

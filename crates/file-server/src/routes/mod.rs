//! HTTP 路由的唯一组装入口。
//!
//! 本模块只维护 method、URL 前缀与 handler 的映射。HTTP 提取与 utoipa 注解
//! 位于 [`crate::handlers`]，wire 契约类型位于 [`crate::models`]，业务实现
//! 位于 [`crate::service`]，跨 crate 共享实现位于 [`crate::ops`]。
//!
//! `/api/v1/userapp` 域已拆出至 file-server-userapp crate（洋葱模型：依赖本 crate
//! 共享设施）；全量/container 形态的 userapp 子树由该 crate 的组装函数提供
//! （`file_server_userapp::full_router` / `container_router`）。

use axum::routing::options;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;
use crate::handlers::{build, build_support, computer, git, health, project, static_files};

/// 业务路由与 OpenAPI 文档的唯一聚合入口（TS 对齐路由族）。
pub fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health::root))
        .routes(routes!(health::health))
        .routes(routes!(health::version))
        .nest("/api/project", project_api_router())
        .nest("/api/git", git_router())
        .nest("/api/build", build_router())
        .nest("/api/computer", computer_router())
        .nest("/api/page", page_router())
}

/// rcoder 主服务合并用的基础路由（全量 [`api_router`] 的子集）。
///
/// 排除项与原因：
/// - `/`、`/health`：与 rcoder 主 Router 的健康检查路由冲突（axum merge 同路径 panic）
/// - `/api/v1/userapp`：userApp 域由 rcoder 侧转发层接管（透传到 per-app 开发容器），
///   本地实现仅存在于开发容器内（file-server-userapp crate）
/// - swagger UI（`/api-docs`）：rcoder 已在 `/api/docs` 聚合 file-server 全量文档
///
/// `/api/version` 保留：无冲突，供调用方探测 file-server 能力版本。
pub fn api_router_base() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health::version))
        .nest("/api/project", project_api_router())
        .nest("/api/git", git_router())
        .nest("/api/build", build_router())
        .nest("/api/computer", computer_router())
        .nest("/api/page", page_router())
}

/// agent-runner 开发容器内嵌形态路由（全量 [`api_router`] 的子集）。
///
/// 与 [`api_router_base`] 的差异：不含 swagger/fallback/`/`、`/health`（与宿主
/// agent_runner 冲突）。`/api/v1/userapp` 子树由调用方（agent_runner embed）经
/// `file_server_userapp::container_router` 追加——开发容器是 userApp 域本地
/// 实现的宿主（曾因误用 base 集导致容器内 /api/v1/userapp/* 全 404）。
pub fn api_router_container() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health::version))
        .nest("/api/project", project_api_router())
        .nest("/api/git", git_router())
        .nest("/api/build", build_router())
        .nest("/api/computer", computer_router())
        .nest("/api/page", page_router())
}

/// `/api/project` + code 路由。
fn project_api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(project::content::get_project_content))
        .routes(routes!(project::crud::delete_project))
        .routes(routes!(project::crud::create_project))
        .routes(routes!(project::crud::copy_project))
        .routes(routes!(project::upload::upload_single_file))
        .routes(routes!(project::upload::upload_batch_files))
        .routes(routes!(project::upload::upload_attachment_file))
        .routes(routes!(project::code::specified_files_update))
        .routes(routes!(project::code::all_files_update))
        .routes(routes!(project::upload::upload_project))
        .routes(routes!(project::version::backup_current_version))
        .routes(routes!(project::version::rollback_version))
        .routes(routes!(project::content::get_project_content_by_version))
        .routes(routes!(project::version::export_project))
        .routes(routes!(project::skills::push_skills_to_workspace))
}

/// `/api/git` 路由。
fn git_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(git::read::branches))
        .routes(routes!(git::read::tags))
        .routes(routes!(git::read::log_history))
        .routes(routes!(git::read::file_content))
        .routes(routes!(git::read::status))
        .routes(routes!(git::write::init))
        .routes(routes!(git::write::add))
        .routes(routes!(git::write::commit))
        .routes(routes!(git::write::unstage))
        .routes(routes!(git::write::discard))
        .routes(routes!(git::write::diff))
        .routes(routes!(git::write::reset))
        .routes(routes!(git::write::checkout))
        .routes(routes!(git::write::revert))
        .routes(routes!(git::refs::branch_create))
        .routes(routes!(git::refs::branch_delete))
        .routes(routes!(git::refs::branch_switch))
        .routes(routes!(git::refs::tag_create))
        .routes(routes!(git::refs::tag_delete))
}

/// `/api/build` 路由。
fn build_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(build::dev::start_dev))
        .routes(routes!(build::dev::stop_dev))
        .routes(routes!(build::dev::restart_dev))
        .routes(routes!(build::dev::list_dev))
        .routes(routes!(build::dev::keep_alive))
        .routes(routes!(build::dev::port_pool_status))
        .routes(routes!(build::logs::get_dev_log))
        .routes(routes!(build::build_exec::build_project))
        .routes(routes!(build_support::parse_build_error))
        .routes(routes!(build::logs::get_log_cache_stats))
        .routes(routes!(build::logs::clear_all_log_cache))
}

/// `/api/computer` 路由，包含静态文件入口。
fn computer_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(computer::files_read::get_file_list))
        .routes(routes!(computer::files_read::resolve_file))
        .routes(routes!(computer::files_read::search_files))
        .routes(routes!(computer::files::delete_workspace))
        .routes(routes!(computer::exec::get_logs))
        .routes(routes!(computer::exec::execute_command))
        .routes(routes!(computer::packages::install_project))
        .routes(routes!(computer::archive::zip_workspace))
        .routes(routes!(computer::archive::download_all_files))
        .routes(routes!(computer::files::files_update))
        .routes(routes!(computer::files::upload::upload_file))
        .routes(routes!(computer::files::upload::upload_files))
        .routes(routes!(computer::files::generate::generate_file))
        .routes(routes!(computer::files::import_project::import_project))
        .routes(routes!(computer::packages::cleanup_build_artifacts))
        .routes(routes!(computer::workspace::create::create_workspace))
        .routes(routes!(computer::workspace::create::create_workspace_v2))
        .routes(routes!(
            computer::workspace::push_skills::push_skills_to_workspace
        ))
        .routes(routes!(
            computer::workspace::push_skills::push_skills_to_workspace_v2
        ))
        .routes(routes!(
            computer::workspace::init_template::init_project_template
        ))
        .routes(routes!(computer::packages::build_agent_package))
        .routes(routes!(static_files::serve_computer))
        .route(
            "/static/{user_id}/{c_id}/{*rest}",
            options(static_files::serve_computer),
        )
}

/// `/api/page/static` 路由。
fn page_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(static_files::serve_page))
        .route(
            "/static/{project_id}/{*rest}",
            options(static_files::serve_page),
        )
}

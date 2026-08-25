//! HTTP 路由的唯一组装入口。
//!
//! 本模块只维护 method、URL 前缀与 handler 的映射。HTTP 提取、DTO、utoipa 注解
//! 位于 [`crate::handlers`]，业务实现位于 [`crate::service`]。

use axum::routing::options;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;
use crate::handlers::{
    build, build_support, computer, git, health, project, static_files, userapp, userapp_app_files,
    userapp_dev, userapp_dev_server, userapp_files,
};

/// 业务路由与 OpenAPI 文档的唯一聚合入口。
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
        .nest("/api/userapp", userapp_router())
}

/// rcoder 主服务合并用的基础路由（全量 [`api_router`] 的子集）。
///
/// 排除项与原因：
/// - `/`、`/health`：与 rcoder 主 Router 的健康检查路由冲突（axum merge 同路径 panic）
/// - `/api/userapp` nest：userApp 域由 rcoder 侧转发层接管（透传到 per-app 开发容器），
///   本地实现仅存在于开发容器内的 file-server
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
/// 与 [`api_router_base`] 的关键差异：**保留 `/api/userapp` nest**——开发容器是
/// userApp 域本地实现的宿主（rcoder 转发层的上游），丢了它容器就失去本职
/// （曾因误用 base 集导致容器内 /api/userapp/* 全 404）。
/// 排除项：`/`、`/health`（与宿主 agent_runner 冲突）、swagger（宿主有自己的文档面）。
pub fn api_router_container() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health::version))
        .nest("/api/project", project_api_router())
        .nest("/api/git", git_router())
        .nest("/api/build", build_router())
        .nest("/api/computer", computer_router())
        .nest("/api/page", page_router())
        .nest("/api/userapp", userapp_router())
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

/// `/api/userapp` 路由（workspace 多项目打包 + 文件操作镜像族 + 取整体包）。
fn userapp_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(userapp::build_workspace))
        .routes(routes!(userapp::get_task))
        .routes(routes!(userapp::get_task_logs))
        .routes(routes!(userapp::stream_task_logs))
        .routes(routes!(userapp::cancel_task))
        .routes(routes!(userapp::detect_project))
        .routes(routes!(userapp::confirm_project))
        .routes(routes!(userapp_files::get_file_list))
        .routes(routes!(userapp_files::resolve_file))
        .routes(routes!(userapp_files::search_files))
        .routes(routes!(userapp_files::files_update))
        .routes(routes!(userapp_files::upload_file))
        .routes(routes!(userapp_files::upload_files))
        .routes(routes!(userapp_files::generate_file))
        .routes(routes!(userapp_files::import_project))
        .routes(routes!(userapp_app_files::upload))
        .routes(routes!(userapp_app_files::upload_from_url))
        .routes(routes!(userapp_app_files::list))
        .routes(routes!(userapp_app_files::delete))
        .routes(routes!(userapp_dev::ensure_workspace))
        .routes(routes!(userapp_dev::execute_command))
        .routes(routes!(userapp_dev::get_logs))
        .routes(routes!(userapp_dev::install_project))
        .routes(routes!(userapp_dev::zip_workspace))
        .routes(routes!(userapp_dev::download_all_files))
        .routes(routes!(userapp_dev::init_project_template))
        .routes(routes!(userapp_dev::push_skills_to_workspace))
        .routes(routes!(userapp_dev_server::dev_start))
        .routes(routes!(userapp_dev_server::dev_stop))
        .routes(routes!(userapp_dev_server::dev_restart))
        .routes(routes!(userapp_dev_server::dev_list))
        .routes(routes!(userapp_dev_server::dev_logs))
        .routes(routes!(static_files::serve_userapp))
        .route(
            "/static/{app_id}/{*rest}",
            options(static_files::serve_userapp),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userapp_routes_are_registered_in_openapi() {
        let document = api_router().into_openapi();
        assert!(document.paths.paths.contains_key("/api/userapp/build"));
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/projects/detect")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/projects/confirm")
        );
        // 开发服务生命周期（start/restart 均为编译+启停异步任务）
        for path in [
            "/api/userapp/dev/start",
            "/api/userapp/dev/stop",
            "/api/userapp/dev/restart",
            "/api/userapp/dev/list",
            "/api/userapp/dev/logs",
        ] {
            assert!(
                document.paths.paths.contains_key(path),
                "userapp dev path missing: {path}"
            );
        }
    }
}

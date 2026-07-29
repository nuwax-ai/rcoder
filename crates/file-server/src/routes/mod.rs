//! HTTP 路由的唯一组装入口。
//!
//! 本模块只维护 method、URL 前缀与 handler 的映射。HTTP 提取、DTO、utoipa 注解
//! 位于 [`crate::handlers`]，业务实现位于 [`crate::service`]。

use axum::routing::options;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;
use crate::handlers::{
    build, build_support, computer, git, health, project, static_files, userapp,
};

/// 业务路由与 OpenAPI 文档的唯一聚合入口。
pub fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health::root))
        .routes(routes!(health::health))
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
        .routes(routes!(project::crud::upload_single_file))
        .routes(routes!(project::crud::upload_batch_files))
        .routes(routes!(project::crud::upload_attachment_file))
        .routes(routes!(project::code::specified_files_update))
        .routes(routes!(project::code::all_files_update))
        .routes(routes!(project::crud::upload_project))
        .routes(routes!(project::version::backup_current_version))
        .routes(routes!(project::version::rollback_version))
        .routes(routes!(project::content::get_project_content_by_version))
        .routes(routes!(project::version::export_project))
        .routes(routes!(project::crud::push_skills_to_workspace))
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
        .routes(routes!(build::start_dev))
        .routes(routes!(build::stop_dev))
        .routes(routes!(build::restart_dev))
        .routes(routes!(build::list_dev))
        .routes(routes!(build::keep_alive))
        .routes(routes!(build::port_pool_status))
        .routes(routes!(build::get_dev_log))
        .routes(routes!(build::build_project))
        .routes(routes!(build_support::parse_build_error))
        .routes(routes!(build::get_log_cache_stats))
        .routes(routes!(build::clear_all_log_cache))
}

/// `/api/computer` 路由，包含静态文件入口。
fn computer_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(computer::files::get_file_list))
        .routes(routes!(computer::files::delete_workspace))
        .routes(routes!(computer::exec::get_logs))
        .routes(routes!(computer::exec::execute_command))
        .routes(routes!(computer::exec::install_project))
        .routes(routes!(computer::archive::zip_workspace))
        .routes(routes!(computer::archive::download_all_files))
        .routes(routes!(computer::files::files_update))
        .routes(routes!(computer::files::upload_file))
        .routes(routes!(computer::files::upload_files))
        .routes(routes!(computer::files::import_project))
        .routes(routes!(computer::exec::cleanup_build_artifacts))
        .routes(routes!(computer::workspace::create_workspace))
        .routes(routes!(computer::workspace::create_workspace_v2))
        .routes(routes!(computer::workspace::push_skills_to_workspace))
        .routes(routes!(computer::workspace::push_skills_to_workspace_v2))
        .routes(routes!(computer::workspace::init_project_template))
        .routes(routes!(computer::exec::build_agent_package))
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

/// `/api/userapp` 路由（workspace 多项目打包 + 取整体包）。
fn userapp_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(userapp::build_workspace))
        .routes(routes!(userapp::detect_project))
        .routes(routes!(userapp::confirm_project))
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
    }
}

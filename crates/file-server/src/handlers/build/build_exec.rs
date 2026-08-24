//! build 执行 handler: install + build + 拷贝 dist。

use axum::extract::State;
use serde::Serialize;
use serde_json::Value;

use super::super::build_support::{copy_dir_all, normalize_build_base};
use super::{BuildQuery, project_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::pnpm::{self, InstallOptions, LogFiles};

/// build 响应
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildDone {
    pub success: bool,
    pub message: String,
    pub project_id: String,
}

/// `GET /api/build/build` (对齐 nuwax buildProject): install + build + 拷贝 dist。
#[utoipa::path(
    get,
    path = "/build",
    params(BuildQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn build_project(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<BuildDone>, AppError> {
    let path = project_path(&state, &q).await?;
    build_project_impl(&state, &path, &q.project_id, q.base_path.as_deref()).await?;
    Ok(Json(BuildDone {
        success: true,
        message: "Build completed".to_string(),
        project_id: q.project_id.clone(),
    }))
}

/// **web/computer 域**的编译实现（读 package.json 的 scripts.build，
/// 走 pnpm install 与 vite build，产物 dist 拷贝）——本函数就是
/// package.json 引擎本体，仅服务 vite 前端项目（`GET /api/build/build`）。
///
/// UserApp 域（Java/Go 多服务）**不走本函数**：其编译入口是 manifest 驱动
/// 的 `service::userapp::build_workspace_package`（project.manifest.toml 的
/// [build].command，/api/userapp/build 与 /api/userapp/dev/rebuild 共用）。
pub(crate) async fn build_project_impl(    state: &AppState,
    path: &std::path::Path,
    project_id: &str,
    base_path: Option<&str>,
) -> Result<(), AppError> {
    if !path.exists() {
        return Err(AppError::resource("project does not exist"));
    }
    let log_dir = crate::service::dev_server::log::log_dir(&state.config, project_id);
    tokio::fs::create_dir_all(&log_dir)
        .await
        .map_err(|e| AppError::system(format!("create build log dir: {e}")))?;
    let now = crate::service::dev_server::now_ms();
    let main_log = log_dir.join(crate::service::dev_server::log::main_log_name());
    let temp_log = log_dir.join(crate::service::dev_server::log::temp_log_name(now));
    let timeout = state.config.dev_command_timeout_secs;

    // 读 scripts.build
    let pkg_content = tokio::fs::read_to_string(path.join("package.json"))
        .await
        .map_err(|e| AppError::business(format!("read package.json: {e}")))?;
    let pkg: Value = serde_json::from_str(&pkg_content)
        .map_err(|e| AppError::business(format!("parse package.json: {e}")))?;
    let build_script = pkg
        .get("scripts")
        .and_then(|s| s.get("build"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::business("Project missing build script"))?;
    // basePath 规范化 (补首尾 /, 对齐 nuwax; vite --base 需尾斜杠)
    let base = normalize_build_base(base_path);

    // 并发控制: 全局信号量 + 项目级互斥 (对齐 nuwax buildingProjects + MAX_BUILD_CONCURRENCY)
    // 立即拒绝超容量/同项目重复 build；guard 在所有退出路径自动释放。
    let _build_guard = state.build_manager.try_start(project_id)?;

    // install (对齐 nuwax: 失败则整体 build 失败, 透传 "Dependency installation failed")
    let install_logs = LogFiles::new(&main_log, &temp_log);
    pnpm::install(
        path,
        &InstallOptions::prefer_offline(),
        Some(&install_logs),
        timeout,
    )
    .await
    .map_err(|e| AppError::system(format!("Dependency installation failed: {e}")))?;
    // build (vite: pnpm exec vite build --base X; 否则 pnpm run build)
    let build_args: Vec<&str> = if build_script.to_ascii_lowercase().contains("vite") {
        vec!["exec", "vite", "build", "--base", &base, "--debug"]
    } else {
        vec!["run", "build"]
    };
    let build_result = crate::service::dev_server::process::run_command_to_log(
        "pnpm",
        &build_args,
        path,
        &main_log,
        &temp_log,
        timeout,
        None,
    )
    .await;
    if let Err(build_error) = build_result {
        // 失败: 读 build 日志用 build_error 解析友好消息 (对齐 nuwax BuildErrorParser)
        let log_content = crate::service::fs_util::read_to_string_bounded(
            &temp_log,
            state.config.log_read_max_bytes,
            "build log",
        )
        .await
        .unwrap_or_else(|_| build_error.to_string());
        let friendly = crate::service::build_error::parse(&log_content);
        return Err(AppError::system(friendly));
    }

    // 拷贝 dist → {DIST_TARGET_DIR}/{projectId}/dist/ (Rust fs, 无 rm -rf shell;
    // 错误为类型化 io::Error, 路径经 PathBuf::join 无注入)
    let dst = state.config.dist_target_dir.join(project_id).join("dist");
    let src = path.join("dist");
    if !src.exists() {
        // 无产物视为成功收尾 (对齐旧壳行为), 响应构造归壳层
        tracing::warn!(project_id, path = %src.display(), "build produced no dist directory");
        return Ok(());
    }
    let src2 = src.clone();
    let dst2 = dst.clone();
    tokio::task::spawn_blocking(move || copy_dir_all(&src2, &dst2))
        .await
        .map_err(|e| AppError::system(format!("copy dist join: {e}")))??;
    Ok(())
}

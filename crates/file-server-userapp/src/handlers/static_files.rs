//! `GET|OPTIONS /api/userapp/static/{app_id}`——按 app 直下最新构建整体包。

use std::path::Path;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use file_server::handlers::static_files::{COMPUTER_CORS, cors_404, serve_from_root};
use file_server::workspace::resolve_userapp_dev;

use crate::UserAppState;
use file_server::extract::AppPath as AxumPath;

/// 下载该 app **最新构建整体包**。
///
/// 调用方只按 app 定位（产物就是每次构建出的一个整体 zip，无需传文件路径）：
/// 服务端在 `{ws}/builds/` 下选 `workspace-package-*.zip` 文件名字典序最大者——
/// 文件名含 UUIDv7（时间有序），字典序最大即最新构建产物；zip 写入经
/// part+rename 原子落盘（见 assemble），目录内不存在半截文件。
///
/// `app_id` 定位走 UserApp 开发卷（`resolve_userapp_dev`，与 build/detect/confirm 同根）。
/// 用 COMPUTER_CORS（暴露 Range/Content-Range，支持大产物断点续传）。
#[utoipa::path(
    get,
    path = "/static/{app_id}",
    params(
        ("app_id" = String, Path, description = "UserApp identifier (= workspace app_id)")
    ),
    responses(
        (status = 200, description = "Latest build artifact zip", body = file_server::openapi::BinaryFile, content_type = "application/zip"),
        (status = 404, description = "No completed build artifact for this app")
    ),
    tag = "UserApp · 开发与构建"
)]
pub async fn serve_userapp(
    State(state): State<UserAppState>,
    AxumPath(app_id): AxumPath<String>,
    req: Request,
) -> Response {
    if app_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    let root = match resolve_userapp_dev(&app_id, None, &state.fs.config) {
        Ok(root) => root,
        Err(error) => return error.into_response(),
    };
    let Some(latest) = latest_build_artifact(&root) else {
        return cors_404(&req, &COMPUTER_CORS);
    };
    // 相对路径由服务端拼装（非用户输入），走公共 serve 逻辑复用 Range/CORS/OPTIONS
    serve_from_root(&root, &latest, &COMPUTER_CORS, req).await
}

/// 选 workspace 内最新构建产物（`builds/` 下 `workspace-package-*.zip` 字典序最大，
/// UUIDv7 文件名字典序 = 构建时间序）。无产物返回 None。
fn latest_build_artifact(ws_root: &Path) -> Option<String> {
    use crate::service::userapp::{WORKSPACE_BUILDS_DIR, WORKSPACE_PACKAGE_PREFIX};

    let builds_dir = ws_root.join(WORKSPACE_BUILDS_DIR);
    let entries = std::fs::read_dir(&builds_dir).ok()?;
    let mut latest: Option<String> = None;
    for entry in entries.flatten() {
        // 只认普通文件（防同名目录混入候选）
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(WORKSPACE_PACKAGE_PREFIX) || !name.ends_with(".zip") {
            continue;
        }
        if latest.as_deref().is_none_or(|cur| name.as_str() > cur) {
            latest = Some(name);
        }
    }
    latest.map(|name| format!("{WORKSPACE_BUILDS_DIR}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_build_artifact_picks_newest_zip_ignoring_part_and_noise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("builds")).expect("mkdir builds");
        // UUIDv7 simple（32 hex）文件名：字典序 = 时间序
        let older = "workspace-package-01999999zzzzeeee1111222233334444.zip";
        let newer = "workspace-package-01999999zzzzeeee1111222233334445.zip";
        for name in [older, newer] {
            std::fs::write(ws.join("builds").join(name), b"zip").expect("write");
        }
        // 干扰项：写一半的 part / 无关前缀 / 非 zip / 子目录——均不参与选包
        std::fs::write(
            ws.join("builds")
                .join("workspace-package-01999999zzzzeeee1111999999999999.zip.part"),
            b"half",
        )
        .expect("write part");
        std::fs::write(ws.join("builds").join("notes.txt"), b"").expect("write noise");
        std::fs::write(ws.join("builds").join("other-1.zip"), b"").expect("write other");
        std::fs::create_dir(ws.join("builds").join("workspace-package-dir.zip"))
            .expect("mkdir trap");

        assert_eq!(
            latest_build_artifact(ws).as_deref(),
            Some("builds/workspace-package-01999999zzzzeeee1111222233334445.zip")
        );
    }

    #[test]
    fn latest_build_artifact_empty_when_no_builds() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(latest_build_artifact(dir.path()), None);
        // 目录不存在 / 有目录但无产物，同样 None
        std::fs::create_dir_all(dir.path().join("builds")).expect("mkdir");
        assert_eq!(latest_build_artifact(dir.path()), None);
    }
}

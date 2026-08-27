//! `GET|OPTIONS /api/v1/userapp/static/{app_id}`——按 app 直下构建整体包
//!（缺省最新产物，可选 `?releaseId=` 按版本精确定位）。

use std::path::Path;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use file_server::handlers::static_files::{COMPUTER_CORS, cors_404, serve_from_root};
use file_server::workspace::resolve_userapp_dev;
use serde::Deserialize;

use crate::UserAppState;
use file_server::extract::AppPath as AxumPath;
use file_server::extract::AppQuery;

/// static 取包 query（`GET /static/{appId}`）。
///
/// `parameter_in` 必须显式声明：utoipa-axum 从 handler 签名自动发现 Query struct
/// 时按 Path extractor 推断 in（会把 query 字段误标 path——swagger 对接即错），
/// 容器级显式声明优先于自动推断。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct StaticQuery {
    /// 可选：按 release_id 精确取包（定位 `builds/workspace-package-{releaseId}.zip`）。
    /// 缺省 = 最新产物。release_id 只允许字母数字与连字符（服务端生成的 UUID 形态），
    /// 其余字符一律拒绝（防路径注入）；指定的版本不存在时 404。
    #[serde(default)]
    pub release_id: Option<String>,
}

/// 下载构建整体包
///
/// 缺省最新，`?releaseId=` 指定版本。
/// 调用方只按 app 定位（产物就是每次构建出的一个整体 zip，无需传文件路径）：
/// - 不带 `releaseId`：服务端在 `{ws}/builds/` 下选 `workspace-package-*.zip`
///   文件名字典序最大者——文件名含 UUIDv7（时间有序），字典序最大即最新构建产物；
///   zip 写入经 part+rename 原子落盘（见 assemble），目录内不存在半截文件。
/// - 带 `releaseId`：精确定位该版本的产物文件（回滚/比对指定版本用）。
///
/// `app_id` 定位走 UserApp 开发卷（`resolve_userapp_dev`，与 build/detect/confirm 同根）。
/// 用 COMPUTER_CORS（暴露 Range/Content-Range，支持大产物断点续传）。
#[utoipa::path(
    get,
    path = "/static/{app_id}",
    params(
        ("app_id" = String, Path, description = "UserApp identifier (= workspace app_id)"),
        StaticQuery,
    ),
    responses(
        (status = 200, description = "Build artifact zip（缺省最新产物；?releaseId= 指定版本）", body = file_server::openapi::BinaryFile, content_type = "application/zip"),
        (status = 404, description = "无产物，或 ?releaseId= 指定的版本不存在（含非法字符被拒）")
    ),
    tag = "UserApp · 开发与构建"
)]
pub async fn serve_userapp(
    State(state): State<UserAppState>,
    AxumPath(app_id): AxumPath<String>,
    AppQuery(q): AppQuery<StaticQuery>,
    req: Request,
) -> Response {
    if app_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    let root = match resolve_userapp_dev(&app_id, None, &state.fs.config) {
        Ok(root) => root,
        Err(error) => return error.into_response(),
    };
    // 空串视同未传（保持"最新产物"语义，避免 ?releaseId= 误触 404）
    let release_id = q
        .release_id
        .as_deref()
        .map(str::trim)
        .filter(|rid| !rid.is_empty());
    let Some(artifact) = resolve_build_artifact(&root, release_id) else {
        return cors_404(&req, &COMPUTER_CORS);
    };
    // 相对路径由服务端拼装（非用户输入），走公共 serve 逻辑复用 Range/CORS/OPTIONS
    serve_from_root(&root, &artifact, &COMPUTER_CORS, req).await
}

/// 解析取包目标：`release_id` 给定 → 精确定位该版本文件；缺省 → 最新产物。
fn resolve_build_artifact(ws_root: &Path, release_id: Option<&str>) -> Option<String> {
    match release_id {
        Some(rid) => exact_build_artifact(ws_root, rid),
        None => latest_build_artifact(ws_root),
    }
}

/// 按 release_id 精确定位产物。release_id 是用户输入——白名单字符集
/// （字母数字+连字符，覆盖 UUID 两种形态）防路径注入：`..`、`/`、后缀注入均被拒。
fn exact_build_artifact(ws_root: &Path, release_id: &str) -> Option<String> {
    use crate::service::userapp::workspace_artifact_rel_path;

    if !release_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let rel = workspace_artifact_rel_path(release_id);
    ws_root.join(&rel).is_file().then_some(rel)
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

    fn ws_with_release(release_id: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("builds")).expect("mkdir builds");
        std::fs::write(
            dir.path()
                .join("builds")
                .join(format!("workspace-package-{release_id}.zip")),
            b"zip",
        )
        .expect("write artifact");
        dir
    }

    #[test]
    fn resolve_build_artifact_exact_release_when_given_latest_when_absent() {
        let dir = ws_with_release("01999999zzzzeeee1111222233334444");
        // 精确命中：返回拼装好的相对路径（与 latest 分支同形态）
        assert_eq!(
            resolve_build_artifact(dir.path(), Some("01999999zzzzeeee1111222233334444")).as_deref(),
            Some("builds/workspace-package-01999999zzzzeeee1111222233334444.zip")
        );
        // 缺省 = 最新产物（同一文件，走 latest 分支）
        assert_eq!(
            resolve_build_artifact(dir.path(), None).as_deref(),
            Some("builds/workspace-package-01999999zzzzeeee1111222233334444.zip")
        );
        // 指定不存在的版本 → None（handler 转 404）
        assert_eq!(
            resolve_build_artifact(dir.path(), Some("00000000000000000000000000000000")),
            None
        );
    }

    #[test]
    fn exact_build_artifact_rejects_path_injection_shapes() {
        let dir = ws_with_release("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // 白名单外的形态一律拒绝：目录穿越 / 后缀注入 / 点 / 空白
        for evil in [
            "../../etc/passwd",
            "aaaa.zip.part",
            "..",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
            "aaa/bbb",
        ] {
            assert_eq!(
                exact_build_artifact(dir.path(), evil),
                None,
                "evil: {evil:?}"
            );
        }
        // 白名单内（连字符形态 UUID）正常命中
        assert_eq!(
            exact_build_artifact(dir.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").as_deref(),
            Some("builds/workspace-package-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.zip")
        );
    }
}

//! `GET|OPTIONS /api/v1/userapp/static/{app_id}`——按 app 直下构建整体包
//!（缺省最新产物，可选 `?release_id=` 按版本精确定位）；
//! `GET /api/v1/userapp/static/{app_id}/meta`——同一目标的元数据（下载前预检，
//! 调用方按 size 做下载阈值判断，避免十 GB 级产物先拉流再拦截）。

use std::path::Path;

use crate::UserAppState;
use crate::error::{UserAppError, success_reply};
use crate::models::StaticQuery;
use crate::models::response::StaticArtifactMeta;
use axum::Json;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use file_server::error::AppError;
use file_server::extract::AppPath as AxumPath;
use file_server::extract::AppQuery;
use file_server::ops::static_share::{COMPUTER_CORS, cors_404, serve_from_root};
use file_server::workspace::resolve_userapp_dev;
use shared_types::HttpResult;

/// 下载构建整体包
///
/// 缺省最新，`?release_id=` 指定版本。
/// 调用方只按 app 定位（产物就是每次构建出的一个整体 zip，无需传文件路径）：
/// - 不带 `release_id`：服务端在 `{ws}/builds/` 下选 `workspace-package-*.zip`
///   文件名字典序最大者——文件名含 UUIDv7（时间有序），字典序最大即最新构建产物；
///   zip 写入经 part+rename 原子落盘（见 assemble），目录内不存在半截文件。
/// - 带 `release_id`：精确定位该版本的产物文件（回滚/比对指定版本用）。
///
/// `app_id` 定位走 Userapp 开发卷（`resolve_userapp_dev`，与 build/detect/confirm 同根）。
/// 用 COMPUTER_CORS（暴露 Range/Content-Range，支持大产物断点续传）。
#[utoipa::path(
    get,
    path = "/static/{app_id}",
    params(
        ("app_id" = String, Path, description = "Userapp identifier (= workspace app_id)"),
        StaticQuery,
    ),
    responses(
        (status = 200, description = "Build artifact zip（缺省最新产物；?release_id= 指定版本）", body = file_server::models::BinaryFile, content_type = "application/zip"),
        (status = 404, description = "无产物，或 ?release_id= 指定的版本不存在（含非法字符被拒）")
    ),
    tag = "Userapp · dev · 构建任务"
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
    // 空串视同未传（保持"最新产物"语义，避免 ?release_id= 误触 404）
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

/// 构建产物元数据（下载前预检）
///
/// 与下载接口同参（path `app_id` + 可选 `?release_id=`）、同源定位（缺省最新
/// 产物），只把文件服务换成一次 metadata 读取——不打开文件、不拉流，供调用
/// 方按 `size_bytes` 做下载阈值判断（十 GB 级产物先拉流再拦截不可接受）。
///
/// 回传服务端实际解析出的 `release_id`：预检与放行下载之间可能落入新构建
/// （缺省语义每次请求重新解析最新产物），调用方把下载 URL 钉死为
/// `?release_id={meta 返回值}` 即可保证两者指向同一份文件。
///
/// 错误形态与域内 JSON 接口一致：HTTP 200 + HttpResult 错误信封（无产物 /
/// 指定版本不存在均为 ERR_NOT_FOUND，message 区分两者），非下载接口的裸 404。
#[utoipa::path(
    get,
    path = "/static/{app_id}/meta",
    params(
        ("app_id" = String, Path, description = "Userapp identifier (= workspace app_id)"),
        StaticQuery,
    ),
    responses(
        (status = 200, body = HttpResult<StaticArtifactMeta>, description = "产物元数据：release_id（服务端实际解析出的版本，调用方应将其钉进后续下载 URL 防预检-下载竞态）、artifact_path、file_name、size_bytes（字节）、mtime_ms（epoch 毫秒）。无产物 / ?release_id= 指定版本不存在：HTTP 200 + ERR_NOT_FOUND 错误信封（message 区分两者）"),
    ),
    tag = "Userapp · dev · 构建任务"
)]
pub async fn artifact_meta(
    State(state): State<UserAppState>,
    AxumPath(app_id): AxumPath<String>,
    AppQuery(q): AppQuery<StaticQuery>,
) -> Result<Json<HttpResult<StaticArtifactMeta>>, UserAppError> {
    if app_id.trim().is_empty() {
        return Err(AppError::validation("app_id must not be blank").into());
    }
    let root = resolve_userapp_dev(&app_id, None, &state.fs.config)?;
    // 空串视同未传（与下载接口同语义，避免 ?release_id= 误触 not-found）
    let release_id = q
        .release_id
        .as_deref()
        .map(str::trim)
        .filter(|rid| !rid.is_empty());
    let Some(artifact) = resolve_build_artifact(&root, release_id) else {
        return Err(AppError::resource(no_artifact_message(release_id, &app_id)).into());
    };
    // resolve 只证明相对路径可解析（exact 分支已 is_file、latest 分支只认普通
    // 文件），此处 stat 拿 size/mtime——与下载接口打开文件同一竞态窗口，
    // NotFound 按"无产物"同语义回，其余 IO 错误如实传播（Fail Fast）。
    let meta = tokio::fs::metadata(root.join(&artifact))
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                AppError::resource(no_artifact_message(release_id, &app_id))
            }
            _ => AppError::file(format!("stat artifact failed: {error}")),
        })?;
    if !meta.is_file() {
        return Err(AppError::resource(no_artifact_message(release_id, &app_id)).into());
    }
    let file_name = artifact.rsplit('/').next().unwrap_or(&artifact).to_owned();
    let Some(release_id) = release_id_from_artifact_name(&file_name).map(str::to_owned) else {
        // 结构上不可达（exact 由服务端拼装、latest 按前后缀过滤），命中即内部
        // 一致性破坏——fail fast，不静默拼回传值
        return Err(AppError::system(format!(
            "artifact file name `{file_name}` does not match expected shape"
        ))
        .into());
    };
    let mtime_ms = artifact_mtime_ms(&meta)?;
    Ok(success_reply(StaticArtifactMeta {
        release_id,
        artifact_path: artifact,
        file_name,
        size_bytes: meta.len(),
        mtime_ms,
    }))
}

/// 无产物错误消息：按调用方是否指定版本区分（白名单拒绝的注入形态同样落
/// "指定版本不存在"——与下载接口 404 不泄露存在性的语义一致）。
fn no_artifact_message(release_id: Option<&str>, app_id: &str) -> String {
    match release_id {
        Some(rid) => format!("release `{rid}` artifact not found (app_id={app_id})"),
        None => format!("no build artifact yet (app_id={app_id})"),
    }
}

/// 从产物文件名反剥 release_id（`workspace-package-{release_id}.zip`）。
fn release_id_from_artifact_name(file_name: &str) -> Option<&str> {
    use crate::service::userapp::WORKSPACE_PACKAGE_PREFIX;

    file_name
        .strip_prefix(WORKSPACE_PACKAGE_PREFIX)
        .and_then(|stem| stem.strip_suffix(".zip"))
}

/// mtime → epoch 毫秒。时钟异常（早于 epoch / 平台不支持）如实报错，不用 0
/// 兜底冒充构建时间。
fn artifact_mtime_ms(meta: &std::fs::Metadata) -> file_server::error::AppResult<u64> {
    let modified = meta
        .modified()
        .map_err(|e| AppError::file(format!("artifact mtime unavailable: {e}")))?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|e| AppError::file(format!("artifact mtime before epoch: {e}")))
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
    // NotFound = 尚无产物（合法 None）；其他错误（权限/IO）不能伪装成"无产物"，
    // 至少留 warn 供排障，避免下载 404 与盘故障混淆
    let entries = match std::fs::read_dir(&builds_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                dir = %builds_dir.display(),
                error = %e,
                "read builds dir failed, treating as no artifact"
            );
            return None;
        }
    };
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
    fn release_id_from_artifact_name_strips_prefix_and_suffix() {
        assert_eq!(
            release_id_from_artifact_name("workspace-package-01999999zzzzeeee1111222233334444.zip"),
            Some("01999999zzzzeeee1111222233334444")
        );
        // 非产物形态：None（handler 侧 fail fast，不静默拼回传值）
        for bad in [
            "other.zip",
            "workspace-package-x.tar",
            "workspace-package-.zip.part",
            "",
        ] {
            assert_eq!(release_id_from_artifact_name(bad), None, "bad: {bad:?}");
        }
    }

    #[test]
    fn no_artifact_message_distinguishes_latest_from_exact() {
        assert_eq!(
            no_artifact_message(None, "app-1"),
            "no build artifact yet (app_id=app-1)"
        );
        assert_eq!(
            no_artifact_message(Some("deadbeef"), "app-1"),
            "release `deadbeef` artifact not found (app_id=app-1)"
        );
    }

    /// handler wire 形态：直接调 handler + 错误侧走 IntoResponse 信封（与
    /// userapp 域测试同法——错误也是 HTTP 200 + 错误信封，非下载接口的裸 404）。
    mod meta_wire {
        use super::super::super::userapp_files::tests_support::make_state;
        use super::super::artifact_meta;
        use crate::UserAppState;
        use crate::models::StaticQuery;
        use crate::models::response::StaticArtifactMeta;
        use axum::response::IntoResponse as _;
        use file_server::extract::{AppPath, AppQuery};

        async fn meta(
            state: &UserAppState,
            app_id: &str,
            release_id: Option<&str>,
        ) -> (bool, String, Option<StaticArtifactMeta>) {
            let response = artifact_meta(
                axum::extract::State(state.clone()),
                AppPath(app_id.to_owned()),
                AppQuery(StaticQuery {
                    release_id: release_id.map(str::to_owned),
                }),
            )
            .await;
            match response {
                Ok(json) => {
                    let payload = json.0;
                    (payload.is_success(), payload.message.clone(), payload.data)
                }
                Err(error) => {
                    let response = error.into_response();
                    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                        .await
                        .expect("body");
                    let envelope: serde_json::Value =
                        serde_json::from_slice(&bytes).expect("envelope");
                    (
                        envelope["code"] == shared_types::error_codes::SUCCESS,
                        envelope["message"].as_str().expect("message").to_owned(),
                        None,
                    )
                }
            }
        }

        fn ws_with_artifacts(root: &std::path::Path, app_id: &str, names: &[&str]) {
            let builds = root.join(app_id).join("builds");
            std::fs::create_dir_all(&builds).expect("mkdir builds");
            for name in names {
                std::fs::write(builds.join(name), b"zip-payload-0123456789").expect("write");
            }
        }

        const OLDER: &str = "workspace-package-01999999zzzzeeee1111222233334444.zip";
        const NEWER: &str = "workspace-package-01999999zzzzeeee1111222233334445.zip";

        #[tokio::test]
        async fn latest_meta_reports_newest_artifact_and_size() {
            let tmp = tempfile::tempdir().expect("tempdir");
            ws_with_artifacts(tmp.path(), "app-1", &[OLDER, NEWER]);
            let state = make_state(tmp.path().to_path_buf());
            let (ok, _message, data) = meta(&state, "app-1", None).await;
            assert!(ok);
            let data = data.expect("meta data");
            // 缺省 = 最新产物，且 release_id 回传解析结果（调用方钉版本用）
            assert_eq!(data.release_id, "01999999zzzzeeee1111222233334445");
            assert_eq!(data.file_name, NEWER);
            assert_eq!(data.artifact_path, format!("builds/{NEWER}"));
            // 内容字节数（b"zip-payload-0123456789" = 12+10 = 22 字节）
            assert_eq!(data.size_bytes, 22);
            assert!(data.mtime_ms > 0, "mtime must be a real epoch ms");
        }

        #[tokio::test]
        async fn exact_release_meta_pins_requested_version() {
            let tmp = tempfile::tempdir().expect("tempdir");
            ws_with_artifacts(tmp.path(), "app-1", &[OLDER, NEWER]);
            let state = make_state(tmp.path().to_path_buf());
            let (ok, _message, data) =
                meta(&state, "app-1", Some("01999999zzzzeeee1111222233334444")).await;
            assert!(ok);
            assert_eq!(
                data.expect("meta data").release_id,
                "01999999zzzzeeee1111222233334444"
            );
        }

        #[tokio::test]
        async fn no_artifact_is_not_found_envelope_with_distinct_messages() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = make_state(tmp.path().to_path_buf());
            // 尚无构建产物
            let (ok, message, data) = meta(&state, "app-1", None).await;
            assert!(!ok);
            assert!(data.is_none());
            assert_eq!(message, "no build artifact yet (app_id=app-1)");

            // 指定版本不存在（含注入形态——与下载 404 同语义，不泄露存在性）
            for rid in ["00000000000000000000000000000000", "../../etc/passwd"] {
                let (ok, message, data) = meta(&state, "app-1", Some(rid)).await;
                assert!(!ok, "rid: {rid}");
                assert!(data.is_none(), "rid: {rid}");
                assert!(
                    message.contains("artifact not found"),
                    "rid: {rid}, message: {message}"
                );
            }
        }

        #[test]
        fn blank_app_id_is_rejected() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = make_state(tmp.path().to_path_buf());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let (ok, _message, data) = rt.block_on(meta(&state, "  ", None));
            assert!(!ok);
            assert!(data.is_none());
        }

        /// 真实路由冒烟：完整公开 URI（nest 前缀 + envelope 中间件路径）可达，
        /// 成功信封形态与字段直读断言一致（防注解路径与实际路由漂移）。
        #[tokio::test]
        async fn full_uri_routes_through_real_router() {
            use axum::body::Body;
            use axum::http::Request;
            use tower::ServiceExt as _;

            let tmp = tempfile::tempdir().expect("tempdir");
            let ws = tmp.path().join("app-1");
            std::fs::create_dir_all(ws.join("builds")).expect("mkdir builds");
            std::fs::write(
                ws.join("builds/workspace-package-01999999zzzzeeee1111222233334444.zip"),
                b"zip-payload-0123456789",
            )
            .expect("write artifact");
            let state = make_state(tmp.path().to_path_buf());
            let (router, _) = crate::routes::userapp_top_router().split_for_parts();
            let router = router.with_state(state);
            let response = router
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v1/userapp/static/app-1/meta")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("body");
            let envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("envelope");
            assert!(
                envelope["code"] == shared_types::error_codes::SUCCESS,
                "envelope: {envelope}"
            );
            assert_eq!(
                envelope["data"]["release_id"],
                "01999999zzzzeeee1111222233334444"
            );
            assert_eq!(envelope["data"]["size_bytes"], 22);
            assert_eq!(
                envelope["data"]["artifact_path"],
                "builds/workspace-package-01999999zzzzeeee1111222233334444.zip"
            );
        }
    }

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

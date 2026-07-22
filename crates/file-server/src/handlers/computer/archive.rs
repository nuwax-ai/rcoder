//! computer 打包下载 handlers: zip-workspace / download-all-files + zip 响应辅助。

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::temp_file::{TemporaryFile, TemporaryFileWriter};
use crate::service::zip;

use super::{UserCidQuery, resolve_computer_target, ws_path};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ZipBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
    #[serde(default)]
    exclude_dirs: Option<Vec<String>>,
}

async fn computer_tmp_zip(state: &AppState) -> Result<TemporaryFile, AppError> {
    TemporaryFileWriter::create(
        &state.config.upload_project_dir.join("temp"),
        "computer-download-",
        u64::MAX,
    )
    .await?
    .finish()
    .await
}

/// zip 下载响应头: Content-Type + UTF-8 Content-Disposition (对齐 nuwax `filename` + `filename*`)。
async fn zip_response(filename: &str, archive: TemporaryFile) -> Result<Response, AppError> {
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        filename,
        utf8_percent_encode(filename)
    );
    let body = archive.into_body().await?;
    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition)
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        body,
    )
        .into_response())
}

/// 文件名 RFC 5987 百分号编码 (仅 [A-Za-z0-9-._~] 不编码)。
fn utf8_percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `POST /api/computer/zip-workspace` (对齐 nuwax zipWorkspace):
/// 无顶层前缀; 工作区不存在则报错; 文件名 `${userId}_${cId}.zip` + UTF-8 filename*。
/// 过滤: ZIP_WORKSPACE_EXCLUDE (强制) + 调用方 excludeDirs (补充) 合并, 对任意路径段匹配
/// (目录与文件同集合); 跳过符号链接; **无** dot-segment 过滤 (保留 .gitignore/.npmrc 等)。
#[utoipa::path(
    post,
    path = "/zip-workspace",
    request_body = ZipBody,
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::openapi::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "Computer"
)]
pub(crate) async fn zip_workspace(
    State(state): State<AppState>,
    Json(body): Json<ZipBody>,
) -> Result<Response, AppError> {
    let src = ws_path(&state, &body.user_id, &body.c_id).await?;
    if !src.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let tmp = computer_tmp_zip(&state).await?;
    // mandatory(ZIP_WORKSPACE_EXCLUDE) ∪ extra(调用方 excludeDirs); 同时填 dirs 与 files,
    // 等价 nuwax archive.directory 的 "任一路径段命中集合则跳过" (对目录与文件同集合)。
    let merged: Vec<String> = state
        .config
        .zip_workspace_exclude
        .iter()
        .cloned()
        .chain(body.exclude_dirs.unwrap_or_default())
        .collect();
    let opts = zip::PackOpts {
        exclude_dirs: merged.clone(),
        exclude_files: merged,
        // 不用 pack_download: 关闭 dot-segment/hardlink 过滤 (nuwax zipWorkspace 保留 .gitignore)
        skip_dot_segments: false,
        skip_hardlinks: false,
        path_prefix: None,
    };
    zip::pack_with_opts(src, tmp.path().to_path_buf(), opts).await?;
    let filename = format!("{}_{}.zip", body.user_id, body.c_id);
    zip_response(&filename, tmp).await
}

/// `GET /api/computer/download-all-files` (对齐 nuwax downloadAllFiles):
/// 顶层前缀 `${userId}_${cId}/` + 空 zip 兜底 + 100MB 大小上限 + UTF-8 filename* + customTargetDir。
#[utoipa::path(
    get,
    path = "/download-all-files",
    params(UserCidQuery),
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::openapi::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "Computer"
)]
pub(crate) async fn download_all_files(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Response, AppError> {
    let src = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    let prefix = format!("{}_{}/", q.user_id, q.c_id);
    let filename = format!("{}_{}.zip", q.user_id, q.c_id);
    let tmp = computer_tmp_zip(&state).await?;

    // 工作区不存在 → 空 zip 兜底 (仅含顶层目录条目, 对齐 nuwax)
    if !src.exists() {
        zip::write_empty_zip(tmp.path().to_path_buf(), prefix.clone()).await?;
        return zip_response(&filename, tmp).await;
    }

    // 过滤对齐 nuwax downloadAllFiles: excludeDirs=TRAVERSE_EXCLUDE_DIRS,
    // excludeFiles=CONTENT_TRAVERSE_EXCLUDE_FILES, 叠加 pack_download 的 dot-segment +
    // 符号链接/硬链接过滤。**非** zip_workspace_exclude (后者当文件名匹配, 目录如
    // node_modules/dist 不被排除 → zip 爆体积)。
    let opts = zip::PackOpts {
        exclude_dirs: state.config.traverse_exclude_dirs.clone(),
        exclude_files: state.config.content_traverse_exclude_files.clone(),
        path_prefix: Some(prefix),
        ..Default::default()
    };
    // 大小上限 (对齐 nuwax DOWNLOAD_MAX_FILE_SIZE_BYTES, 默认 100MB)
    let max = state.config.download_max_file_size_bytes;
    let src_for_size = src.clone();
    let opts_for_size = opts.clone();
    let size = tokio::task::spawn_blocking(move || {
        zip::downloadable_size_blocking(&src_for_size, &opts_for_size)
    })
    .await
    .map_err(|e| AppError::system(format!("size calc join: {e}")))?;
    if size > max {
        let cur_mb = size / 1024 / 1024;
        let max_mb = max / 1024 / 1024;
        return Err(AppError::validation(format!(
            "Download failed: total file size {cur_mb}MB exceeds limit {max_mb}MB"
        )));
    }

    zip::pack_download(src, tmp.path().to_path_buf(), opts).await?;
    zip_response(&filename, tmp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_percent_encode_only_safe_chars_pass() {
        // [A-Za-z0-9-._~] 保留, 其余百分号编码
        assert_eq!(utf8_percent_encode("a-b.c_1~2.zip"), "a-b.c_1~2.zip");
        assert_eq!(utf8_percent_encode("a b.zip"), "a%20b.zip");
        // 中文: 中=E4%B8%AD, 文=E6%96%87
        assert_eq!(utf8_percent_encode("中文.zip"), "%E4%B8%AD%E6%96%87.zip");
    }
}

//! 打包下载共享实现：zip-workspace / download-all-files。
//!
//! 壳在 handlers/computer/archive.rs；utf8_percent_encode 的单测随 helper
//! 留在本文件。

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::AppError;
use crate::service::temp_file::{TemporaryFile, TemporaryFileWriter};
use crate::service::zip;

pub(super) async fn computer_tmp_zip(state: &AppState) -> Result<TemporaryFile, AppError> {
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
pub(super) async fn zip_response(
    filename: &str,
    archive: TemporaryFile,
) -> Result<Response, AppError> {
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

/// zip-workspace 的 workspace 无关实现 (filename 由壳层拼: computer=`{user}_{cId}.zip`)。
pub async fn zip_workspace_impl(
    state: &AppState,
    src: std::path::PathBuf,
    extra_exclude_dirs: Vec<String>,
    filename: String,
) -> Result<Response, AppError> {
    if !src.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let tmp = computer_tmp_zip(state).await?;
    // mandatory(ZIP_WORKSPACE_EXCLUDE) ∪ extra(调用方 excludeDirs); 同时填 dirs 与 files,
    // 等价 nuwax archive.directory 的 "任一路径段命中集合则跳过" (对目录与文件同集合)。
    let merged: Vec<String> = state
        .config
        .zip_workspace_exclude
        .iter()
        .cloned()
        .chain(extra_exclude_dirs)
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
    zip_response(&filename, tmp).await
}

/// download-all-files 的 workspace 无关实现 (prefix/filename 由壳层拼:
/// computer=`{user}_{cId}/` 与 `{user}_{cId}.zip`)。
pub async fn download_all_files_impl(
    state: &AppState,
    src: std::path::PathBuf,
    prefix: String,
    filename: String,
) -> Result<Response, AppError> {
    let tmp = computer_tmp_zip(state).await?;

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

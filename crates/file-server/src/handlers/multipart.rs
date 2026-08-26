//! HTTP multipart 边界层共享读取与校验。

use std::path::Path;

use axum::extract::multipart::Field;

use crate::error::{AppError, AppResult};
use crate::service::temp_file::{TemporaryFile, TemporaryFileWriter};

const MAX_TEXT_FIELD_BYTES: usize = 1024 * 1024;

pub async fn text_field(mut field: Field<'_>) -> AppResult<String> {
    let mut data = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|error| AppError::validation(format!("read multipart field: {error}")))?
    {
        let next = data
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| AppError::validation("multipart text field size overflow"))?;
        if next > MAX_TEXT_FIELD_BYTES {
            return Err(AppError::validation(format!(
                "multipart text field exceeds limit (max {MAX_TEXT_FIELD_BYTES} bytes)"
            )));
        }
        data.try_reserve(chunk.len())
            .map_err(|error| AppError::system(format!("reserve multipart text field: {error}")))?;
        data.extend_from_slice(&chunk);
    }
    String::from_utf8(data).map_err(|error| {
        AppError::validation(format!("multipart text field is not UTF-8: {error}"))
    })
}

pub async fn file_field(
    mut field: Field<'_>,
    max_bytes: u64,
    temp_dir: &Path,
) -> AppResult<TemporaryFile> {
    let mut writer = TemporaryFileWriter::create(temp_dir, "multipart-", max_bytes).await?;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|error| AppError::validation(format!("read multipart file: {error}")))?
    {
        writer.write(&chunk).await?;
    }
    writer.finish().await
}

/// `.zip` 扩展名校验（对齐 nuwax multer fileFilter）。
pub fn validate_zip_ext(filename: Option<&str>) -> AppResult<()> {
    let is_zip = filename
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
    if is_zip {
        Ok(())
    } else {
        Err(AppError::validation("Only zip files are supported"))
    }
}

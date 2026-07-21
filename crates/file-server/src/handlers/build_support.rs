//! Build handler 的文件系统与错误解析职责。

use axum::extract::State;
use serde::Deserialize;

use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;

use super::build::response;

pub(super) fn normalize_build_base(base: Option<&str>) -> String {
    let base = base.map(str::trim).unwrap_or("/").to_string();
    if base.is_empty() {
        return "/".to_string();
    }
    let mut normalized = if base.starts_with('/') {
        base
    } else {
        format!("/{base}")
    };
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

pub(super) fn copy_dir_all(
    source: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), AppError> {
    use std::fs;
    if target.exists() {
        fs::remove_dir_all(target).map_err(|error| {
            AppError::system(format!("remove old dist {}: {error}", target.display()))
        })?;
    }
    fs::create_dir_all(target)
        .map_err(|error| AppError::system(format!("create dist {}: {error}", target.display())))?;
    for entry in fs::read_dir(source)
        .map_err(|error| AppError::system(format!("read dist {}: {error}", source.display())))?
    {
        let entry = entry.map_err(|error| AppError::system(format!("read dir entry: {error}")))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::system(format!("file type: {error}")))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|error| AppError::system(format!("copy {}: {error}", from.display())))?;
        }
    }
    Ok(())
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParseErrorBody {
    #[allow(dead_code)]
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    project_id: String,
    error_message: String,
}

#[utoipa::path(post, path = "/parse-build-error", request_body = ParseErrorBody, responses(crate::openapi::JsonApiResponses), tag = "Build")]
pub(crate) async fn parse_build_error(
    State(_state): State<AppState>,
    Json(body): Json<ParseErrorBody>,
) -> Result<Json<response::Simple>, AppError> {
    let message = crate::service::build_error::parse(&body.error_message);
    Ok(Json(response::Simple {
        success: true,
        message,
    }))
}

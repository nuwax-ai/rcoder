//! import-project handler: zip 上传 → 解压合并到工作区。

use axum::extract::State;
use garde::Validate;
use serde_json::{Value, json};

use super::super::{file_field, resolve_computer_target, text_field, validate_zip_ext};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::service::temp_file::TemporaryFile;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

/// import-project 必填字段 (userId/cId 必填非空 + zip 文件必填)。
#[derive(garde::Validate)]
struct ImportProjectFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

/// [`ImportProjectFields`] 校验后的全必填形态。
struct ValidatedImportProject {
    user_id: String,
    cid: String,
    data: TemporaryFile,
}

impl ImportProjectFields {
    fn into_validated(self) -> Result<ValidatedImportProject, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        Ok(ValidatedImportProject {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
            data: self
                .data
                .ok_or_else(|| AppError::system("zip file missing after garde validation"))?,
        })
    }
}

/// 导入项目 zip
///
/// 对齐 nuwax computer importProject:
/// 上传 zip → 解压 + removeTopLevelDir + 白名单保留合并到工作区。
#[utoipa::path(post, path = "/import-project", request_body(content = ImportProjectForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut data = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            _ => {}
        }
    }
    let fields = ImportProjectFields { user_id, cid, data };
    let v = fields.into_validated()?;
    validate_zip_ext(file_name.as_deref())?;
    let target_dir =
        resolve_computer_target(&state, &v.user_id, &v.cid, custom_target_dir.as_deref()).await?;
    let target = import_project_impl(target_dir, v.data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": v.user_id,
        "cId": v.cid,
        "targetDir": target,
    })))
}

/// import-project 的 workspace 无关实现：解压合并并返回目标目录（展示/回显归各域壳层）。
pub async fn import_project_impl(
    target_dir: std::path::PathBuf,
    data: TemporaryFile,
) -> Result<String, AppError> {
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data.path()).await?;
    Ok(res.target_dir)
}

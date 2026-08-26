//! init-project-template handler: 模板 zip 解压到工作区 (可选 git init)。

use axum::extract::State;
use garde::Validate;
use serde_json::{Value, json};

use super::super::{file_field, text_field, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::service::temp_file::TemporaryFile;
use crate::service::zip;

/// init-project-template 必填字段 (含模板 zip 文件)。
#[derive(garde::Validate)]
struct InitTemplateFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

/// [`InitTemplateFields`] 校验后的全必填形态。
struct ValidatedInitTemplate {
    user_id: String,
    cid: String,
    data: TemporaryFile,
}

impl InitTemplateFields {
    fn into_validated(self) -> Result<ValidatedInitTemplate, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        Ok(ValidatedInitTemplate {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
            data: self
                .data
                .ok_or_else(|| AppError::system("template zip missing after garde validation"))?,
        })
    }
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitProjectTemplateForm {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: String,
    pub enable_git: Option<bool>,
}

/// `POST /api/computer/init-project-template` (对齐 nuwax initProjectTemplate)。
/// multipart: userId, cId, file(模板 zip), enableGit。解压到工作区。
/// git 触发双开关: GIT_ENABLED && enableGit → init + commit (对齐 nuwax)。
#[utoipa::path(post, path = "/init-project-template", request_body(content = InitProjectTemplateForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn init_project_template(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut data = None;
    let mut enable_git = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            "enableGit" => {
                enable_git = matches!(
                    text_field(field).await?.trim().to_lowercase().as_str(),
                    "true" | "1" | "yes"
                );
            }
            _ => {}
        }
    }
    let fields = InitTemplateFields { user_id, cid, data };
    let v = fields.into_validated()?;
    let ws = ws_path(&state, &v.user_id, &v.cid).await?;
    init_project_template_impl(&state, ws, v.data, enable_git).await
}

/// init-project-template 的 workspace 无关实现。
pub async fn init_project_template_impl(
    state: &AppState,
    ws: std::path::PathBuf,
    data: TemporaryFile,
    enable_git: bool,
) -> Result<Json<Value>, AppError> {
    tokio::fs::create_dir_all(&ws).await?;
    zip::extract_to(data.path().to_path_buf(), ws.clone()).await?;
    // git 双开关: GIT_ENABLED && enableGit → init + initial commit (对齐 nuwax)
    if state.config.git_enabled && enable_git {
        let an = state.config.git_default_author_name.clone();
        let ae = state.config.git_default_author_email.clone();
        // init_repo 内部已含 initial commit (ensure_repo + ensure_gitignore + commit_indexed)
        if let Err(e) = crate::service::git::init_repo(&ws, &an, &ae) {
            tracing::warn!(error = %e, "git init_repo after template init failed (skipping)");
        }
    }
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized successfully",
        "workspaceRoot": ws.display().to_string(),
    })))
}

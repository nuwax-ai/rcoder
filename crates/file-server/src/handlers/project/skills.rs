//! project skills 推送 handler: push-skills-to-workspace。

use axum::extract::State;
use garde::Validate;
use serde_json::json;

use super::{ctx_from, file_field, text_field};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::service::skills as skills_service;

/// project_id 必填 (multipart 提取后构造 + garde 校验)。
#[derive(garde::Validate)]
struct ProjectIdField {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    project_id: Option<String>,
}

/// 校验 project_id 并取数 (multipart 提取后)。
fn require_project_id(project_id: Option<String>) -> Result<String, AppError> {
    let f = ProjectIdField { project_id };
    f.validate().map_err(crate::error::from_garde)?;
    f.project_id
        .ok_or_else(|| AppError::system("project_id missing after garde validation"))
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushProjectSkillsForm {
    pub project_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// `POST /api/project/push-skills-to-workspace`
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = PushProjectSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut zip_data = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut tenant = None;
    let mut space = None;
    let mut iso = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "projectId" => project_id = Some(text_field(field).await?),
            "file" => {
                zip_data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            "skillUrls" => {
                let t = text_field(field).await?;
                // 兼容 JSON 数组字符串 / 单 URL
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let project_id = require_project_id(project_id)?;
    state
        .skill_downloader
        .validate_url_count(skill_urls.len())?;
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = skills_service::push_skills(
        &*state.resolver,
        &ctx,
        zip_data.as_ref().map(|file| file.path()),
        skill_urls,
        &state.skill_downloader,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Skills pushed to workspace",
        "projectPath": result.project_path,
        "updatedSkills": result.updated_skills,
    })))
}

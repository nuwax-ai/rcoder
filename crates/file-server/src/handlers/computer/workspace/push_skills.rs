//! push-skills-to-workspace v1/v2 handlers: 技能推送 (可选 agent-store 路径)。

use axum::extract::State;
use serde_json::{Value, json};

use super::super::{file_field, text_field, ws_path};
use super::require_workspace_fields;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::service::skills as skills_service;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushSkillsForm {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    /// 智能体 ID (有则可能走实体存储; 须同时满足会话已是软链)
    pub agent_id: Option<String>,
}

/// `POST /api/computer/push-skills-to-workspace` (对齐 nuwax pushSkillsToWorkspace;
/// 复用 skills_service::push_skills_at, 推到 .claude/skills + syncAgents)。
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = PushSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    push_skills_to_workspace_impl(state, multipart).await
}

/// 推送 skills 到工作区 v2（多文件上传，软链优先 + copy 回退）
#[utoipa::path(post, path = "/push-skills-to-workspace-v2", request_body(content = PushSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn push_skills_to_workspace_v2(
    State(state): State<AppState>,
    multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    push_skills_to_workspace_impl(state, multipart).await
}

async fn push_skills_to_workspace_impl(
    state: AppState,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut zip_data = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut agent_id: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
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
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "agentId" => agent_id = Some(text_field(field).await?),
            _ => {}
        }
    }
    let (user_id, cid) = require_workspace_fields(user_id, cid)?;
    state
        .skill_downloader
        .validate_url_count(skill_urls.len())?;
    let ws = ws_path(&state, &user_id, &cid).await?;
    if !crate::service::fs_util::path_exists(&ws).await? {
        return Err(AppError::resource("workspace does not exist"));
    }

    // 有 agentId 且 workspace 已软链 → 写 agent-store; 否则旧路径 (对齐 TS pushSkillsToWorkspace)
    let agent_id = agent_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let updated = if let Some(agent_id) = agent_id {
        let skills_path = ws.join(".agents").join("skills");
        if crate::service::agent_store::is_dir_link(&skills_path) {
            let user_root = ws.parent().unwrap_or(&ws).to_path_buf();
            skills_service::push_skills_to_agent_store(skills_service::PushToStoreParams {
                user_root: &user_root,
                cid: &cid,
                agent_id,
                zip_path: zip_data.as_ref().map(|file| file.path()),
                skill_urls,
                downloader: &state.skill_downloader,
            })
            .await?
        } else {
            tracing::info!(
                user_id,
                cid,
                agent_id,
                "push skills: agentId present but workspace not symlinked, use legacy path"
            );
            skills_service::push_skills_at(
                &ws,
                zip_data.as_ref().map(|file| file.path()),
                skill_urls,
                &state.skill_downloader,
            )
            .await?
        }
    } else {
        skills_service::push_skills_at(
            &ws,
            zip_data.as_ref().map(|file| file.path()),
            skill_urls,
            &state.skill_downloader,
        )
        .await?
    };
    // message 对齐 nuwax pushSkillsToWorkspace: 有 skills → "Pushed N skills: a, b";
    // 无 → "No valid skill directories found in file or skillUrls"
    let message = if updated.is_empty() {
        "No valid skill directories found in file or skillUrls".to_string()
    } else {
        format!("Pushed {} skills: {}", updated.len(), updated.join(", "))
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "workspaceRoot": ws.display().to_string(),
        "updatedSkills": updated,
    })))
}

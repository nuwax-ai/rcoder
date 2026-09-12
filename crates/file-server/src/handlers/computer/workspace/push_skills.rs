//! push-skills-to-workspace v1/v2 handlers: 技能推送 (可选 agent-store 路径)。

use axum::extract::State;
use serde_json::Value;

use crate::ops::multipart::{file_field, text_field};
use crate::ops::workspace::push_skills_impl;

use super::super::ws_path;
use super::require_workspace_fields;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::models::PushSkillsForm;

/// 技能推送到工作区
///
/// 对齐 nuwax pushSkillsToWorkspace;
/// 复用 skills_service::push_skills_at, 推到 .claude/skills + syncAgents。
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = PushSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    push_skills_to_workspace_impl(state, multipart).await
}

/// 推送 skills 到工作区
///
/// v2：多文件上传，软链优先 + copy 回退。
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
    let mut workspace_path = None; // 项目绑定目录 (对齐 TS 1.4.5, 可选 multipart 字段)
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
            "workspacePath" => workspace_path = Some(text_field(field).await?),
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
    // URL 数校验先于 ws_path（ws_path 在阶段2 Subvolume resolver 下有 ensure-PVC 副作用,
    // 无效请求不应触发; 对齐抽取前的顺序）
    state
        .skill_downloader
        .validate_url_count(skill_urls.len())?;
    let ws = ws_path(&state, &user_id, &cid, workspace_path.as_deref()).await?;
    // agent-store 根锚定 (绑定目录场景 store 锚定配置根, 对齐 TS 1.4.5; 默认
    // None = ws.parent() 派生)
    let store_root =
        super::super::agent_store_user_root(&state, &user_id, &ws, workspace_path.as_deref());
    push_skills_impl(
        &state,
        crate::ops::workspace::PushSkillsParams {
            ws: &ws,
            cid: &cid,
            zip_data: zip_data.as_ref(),
            skill_urls,
            agent_id: agent_id.as_deref(),
            allow_agent_store: true,
            agent_store_root: Some(&store_root),
        },
    )
    .await
}

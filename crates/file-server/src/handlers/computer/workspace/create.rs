//! create-workspace v1/v2 handlers: 工作区装配 (可选 agent-store 实体存储 + 软链)。

use axum::extract::State;

use crate::ops::multipart::{file_field, text_field, validate_zip_ext};

use super::super::ws_path;
use super::require_workspace_fields;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::models::{CreateWorkspaceForm, CreateWorkspaceResponse, CreateWorkspaceV2Form};

fn create_workspace_response(
    state: &AppState,
    result: crate::service::computer_ws::CreateWorkspaceResult,
) -> Json<CreateWorkspaceResponse> {
    // 对齐 TS: workspaceRoot = COMPUTER_WORKSPACE_DIR (配置值, 不从叶子路径倒推)
    Json(CreateWorkspaceResponse {
        success: true,
        message: result.message,
        workspace_root: state
            .config
            .computer_workspace_dir
            .to_string_lossy()
            .to_string(),
        updated_skills: result.updated_skills,
        failed_skills: result.failed_skills,
    })
}

/// 创建 computer 工作区
///
/// 对齐 nuwax createWorkspace; v1:
/// mkdir 工作区 + `.agents/{skills,agents}` 装配 + 可选 skill zip 合并 + syncAgents。
/// v2 的 agent hook 配置 (claude/codex/opencode mcp/hooks/permissions) 见 create_workspace_v2。
#[utoipa::path(post, path = "/create-workspace", request_body(content = CreateWorkspaceForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn create_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<CreateWorkspaceResponse>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                skill_zip = Some(
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
    let (user_id, cid) = require_workspace_fields(user_id, cid)?;
    if skill_zip.is_some() {
        validate_zip_ext(file_name.as_deref())?;
    }
    let ws = ws_path(&state, &user_id, &cid).await?;
    tokio::fs::create_dir_all(&ws).await?;
    let res = crate::service::computer_ws::create_workspace(
        &ws,
        skill_zip.as_ref().map(|file| file.path()),
        Vec::new(),
        None,
        Some(&state.skill_downloader),
    )
    .await?;
    Ok(create_workspace_response(&state, res))
}

/// 创建工作区 v2
///
/// 对齐 nuwax create-workspace-v2:
/// multipart: userId, cId, file, skillUrls, mcpServersConfig, hooksConfig,
/// permissionsConfig, hookScripts。skillUrls/hookScripts 若为 JSON 字符串则解析。
/// 有 agentId 时走实体存储 + 软链 (create_workspace_with_agent_store)。
#[utoipa::path(post, path = "/create-workspace-v2", request_body(content = CreateWorkspaceV2Form, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn create_workspace_v2(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<CreateWorkspaceResponse>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip = None;
    let mut file_name = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut mcp_servers_config: Option<String> = None;
    let mut hooks_config: Option<String> = None;
    let mut permissions_config: Option<String> = None;
    let mut hook_scripts_raw: Option<String> = None;
    let mut agent_id: Option<String> = None;
    let mut skill_url_map_raw: Option<String> = None;
    let mut skill_names_raw: Option<String> = None;
    let mut update_skill_names_raw: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                skill_zip = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            "skillUrls" => {
                let t = text_field(field).await?;
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "mcpServersConfig" => mcp_servers_config = Some(text_field(field).await?),
            "hooksConfig" => hooks_config = Some(text_field(field).await?),
            "permissionsConfig" => permissions_config = Some(text_field(field).await?),
            "hookScripts" => hook_scripts_raw = Some(text_field(field).await?),
            "agentId" => agent_id = Some(text_field(field).await?),
            "skillUrlMap" => skill_url_map_raw = Some(text_field(field).await?),
            "skillNames" => skill_names_raw = Some(text_field(field).await?),
            "updateSkillNames" => update_skill_names_raw = Some(text_field(field).await?),
            _ => {}
        }
    }
    let (user_id, cid) = require_workspace_fields(user_id, cid)?;
    state
        .skill_downloader
        .validate_url_count(skill_urls.len())?;
    if skill_zip.is_some() {
        validate_zip_ext(file_name.as_deref())?;
    }
    // hookScripts: JSON 字符串 → Vec<HookScript>, 解析失败 → None (对齐 nuwax)
    let hook_scripts = hook_scripts_raw.and_then(|s| {
        serde_json::from_str::<Vec<crate::service::agent_hooks::HookScript>>(&s).ok()
    });
    let hook_input = crate::service::agent_hooks::HookConfigInput {
        mcp_servers_config,
        hooks_config,
        permissions_config,
        hook_scripts,
    };
    // agent-store 参数解析 (JSON 字符串 → 结构化; BTreeMap 保证迭代顺序稳定可复现)
    let skill_url_map = skill_url_map_raw
        .and_then(|s| serde_json::from_str::<std::collections::BTreeMap<String, String>>(&s).ok());
    // skillNames: 显式传入才 prune (Some); 未传或解析失败 → None = 跳过 prune。
    // ⚠️ 与 TS 差异: TS 未传 skillNames 时 prune 会清空全部非 dynamic skill,
    // 此处加固为跳过 — 防客户端漏传字段静默销毁技能库。
    let skill_names = match skill_names_raw {
        Some(s) => match serde_json::from_str::<Vec<String>>(&s) {
            Ok(names) => Some(names),
            Err(e) => {
                tracing::warn!(error = %e, "parse skillNames failed, skipping prune");
                None
            }
        },
        None => None,
    };
    // updateSkillNames: 显式传入 (含空数组) 时按需安装; 未传则全量 (None)
    let update_skill_names =
        update_skill_names_raw.map(|s| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default());

    let ws = ws_path(&state, &user_id, &cid).await?;
    tokio::fs::create_dir_all(&ws).await?;

    // 有 agentId → 走实体存储 + 软链; 否则走旧路径
    let agent_id = agent_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let res = if let Some(agent_id) = agent_id {
        // user_root = 会话工作区父目录 (Local={root}/{userId}, Subvolume=subvolume_base);
        // 两种 resolver 模式下 agent-store 都建在 user_root/.agent-store, 与会话同树
        let user_root = ws.parent().unwrap_or(&ws).to_path_buf();
        crate::service::computer_ws::create_workspace_with_agent_store(
            crate::service::computer_ws::CreateAgentStoreParams {
                user_root: &user_root,
                cid: &cid,
                agent_id,
                skill_zip: skill_zip.as_ref().map(|file| file.path()),
                skill_urls,
                skill_url_map,
                skill_names,
                update_skill_names,
                hook_config: Some(hook_input),
                downloader: Some(&state.skill_downloader),
            },
        )
        .await?
    } else {
        crate::service::computer_ws::create_workspace(
            &ws,
            skill_zip.as_ref().map(|file| file.path()),
            skill_urls,
            Some(hook_input),
            Some(&state.skill_downloader),
        )
        .await?
    };
    Ok(create_workspace_response(&state, res))
}

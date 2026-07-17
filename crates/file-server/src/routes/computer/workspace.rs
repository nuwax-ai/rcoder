//! computer 工作区装配路由: create-workspace / create-workspace-v2 /
//! push-skills-to-workspace / init-project-template。

use axum::Json;
use axum::extract::{Multipart, State};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::service::{skills as skills_service, zip};

use super::{bytes_field, text_field, validate_zip_ext, ws_path};

/// `POST /api/computer/create-workspace` (对齐 nuwax createWorkspace; v1):
/// mkdir 工作区 + `.agents/{skills,agents}` 装配 + 可选 skill zip 合并 + syncAgents。
/// v2 的 agent hook 配置 (claude/codex/opencode mcp/hooks/permissions) 见 create_workspace_v2。
pub(super) async fn create_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip: Option<Vec<u8>> = None;
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
                skill_zip = Some(bytes_field(field).await?);
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if skill_zip.is_some() {
        validate_zip_ext(file_name.as_deref())?;
    }
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    let res =
        crate::service::computer_ws::create_workspace(&ws, skill_zip, Vec::new(), None).await?;
    Ok(Json(json!({
        "success": true,
        "message": res.message,
        "workspaceRoot": res.workspace_root,
        "updatedSkills": res.updated_skills,
    })))
}

/// `POST /api/computer/create-workspace-v2` (对齐 nuwax create-workspace-v2):
/// multipart: userId, cId, file, skillUrls, mcpServersConfig, hooksConfig,
/// permissionsConfig, hookScripts。skillUrls/hookScripts 若为 JSON 字符串则解析。
/// 复用 computer_ws::create_workspace + write_agent_hook_configs。
pub(super) async fn create_workspace_v2(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip: Option<Vec<u8>> = None;
    let mut file_name = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut mcp_servers_config: Option<String> = None;
    let mut hooks_config: Option<String> = None;
    let mut permissions_config: Option<String> = None;
    let mut hook_scripts_raw: Option<String> = None;
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
                skill_zip = Some(bytes_field(field).await?);
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
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
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
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    let res =
        crate::service::computer_ws::create_workspace(&ws, skill_zip, skill_urls, Some(hook_input))
            .await?;
    Ok(Json(json!({
        "success": true,
        "message": res.message,
        "workspaceRoot": res.workspace_root,
        "updatedSkills": res.updated_skills,
    })))
}

/// `POST /api/computer/push-skills-to-workspace` (对齐 nuwax pushSkillsToWorkspace;
/// 复用 skills_service::push_skills_at, 推到 .claude/skills + syncAgents)。
pub(super) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut zip_data: Option<Vec<u8>> = None;
    let mut skill_urls: Vec<String> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => zip_data = Some(bytes_field(field).await?),
            "skillUrls" => {
                let t = text_field(field).await?;
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let ws = ws_path(&state, &user_id, &cid);
    if !tokio::fs::try_exists(&ws).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
    let updated = skills_service::push_skills_at(&ws, zip_data, skill_urls).await?;
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

/// `POST /api/computer/init-project-template` (对齐 nuwax initProjectTemplate)。
/// multipart: userId, cId, file(模板 zip), enableGit。解压到工作区。
/// git 触发双开关: GIT_ENABLED && enableGit → init + commit (对齐 nuwax)。
pub(super) async fn init_project_template(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut data: Option<Vec<u8>> = None;
    let mut enable_git = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => data = Some(bytes_field(field).await?),
            "enableGit" => {
                enable_git = matches!(
                    text_field(field).await?.trim().to_lowercase().as_str(),
                    "true" | "1" | "yes"
                );
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file (template zip) is required"))?;
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    // 解压模板
    let tmp = std::env::temp_dir().join(format!(
        "computer-init-{}-{}-{}.zip",
        user_id,
        cid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    tokio::fs::write(&tmp, data).await?;
    let r = zip::extract_to(tmp.clone(), ws.clone()).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    r?;
    // git 双开关: GIT_ENABLED && enableGit → init + initial commit (对齐 nuwax)
    if state.config.git_enabled && enable_git {
        let an = state.config.git_default_author_name.clone();
        let ae = state.config.git_default_author_email.clone();
        // init_repo 内部已含 initial commit (ensure_repo + ensure_gitignore + commit_indexed)
        let _ = crate::service::git::init_repo(&ws, &an, &ae);
    }
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized successfully",
        "workspaceRoot": ws.display().to_string(),
    })))
}

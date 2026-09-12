//! 工作区装配共享实现：init-project-template / push-skills。
//!
//! 壳在 handlers/computer/workspace/*。

use std::path::Path;

use crate::extract::AppJson as Json;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::service::skills as skills_service;
use crate::service::temp_file::TemporaryFile;
use crate::service::zip;

/// init-project-template 的 workspace 无关核心（返回 workspace 根；
/// 响应拼装归各域壳层）。
pub async fn init_project_template_core(
    state: &AppState,
    ws: std::path::PathBuf,
    data: TemporaryFile,
    enable_git: bool,
) -> Result<std::path::PathBuf, AppError> {
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
    Ok(ws)
}

/// init-project-template 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn init_project_template_impl(
    state: &AppState,
    ws: std::path::PathBuf,
    data: TemporaryFile,
    enable_git: bool,
) -> Result<Json<Value>, AppError> {
    let ws = init_project_template_core(state, ws, data, enable_git).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized successfully",
        "workspaceRoot": ws.display().to_string(),
    })))
}

/// push-skills 结果（updated 为已推送的技能目录名列表）。
pub struct PushedSkills {
    pub updated: Vec<String>,
}

/// push-skills 参数集 (结构化入参, 避免 too_many_arguments; 同
/// `PushToStoreParams` / `CreateAgentStoreParams` 惯例)。
pub struct PushSkillsParams<'a> {
    /// 会话工作区
    pub ws: &'a Path,
    pub cid: &'a str,
    pub zip_data: Option<&'a TemporaryFile>,
    pub skill_urls: Vec<String>,
    pub agent_id: Option<&'a str>,
    /// 是否允许 agent-store 软链分支 (computer 布局下成立; userapp 开发卷
    /// 布局 parent 是共享卷根, 传 false 一律走 legacy)
    pub allow_agent_store: bool,
    /// agent-store 根锚定 (项目绑定目录场景, 对齐 TS 1.4.5 `getAgentStorePath`
    /// ——store 锚定配置根不随绑定漂移); `None` 保持 `ws.parent()` 派生
    /// (默认布局, 与历史行为逐字节一致)。
    pub agent_store_root: Option<&'a Path>,
}

/// push-skills 的 workspace 无关核心（扁平签名, **file-server-userapp 跨 crate
/// 调用方依赖**; agent-store 根固定 `ws.parent()` 派生）。
pub async fn push_skills_core(
    state: &AppState,
    ws: &Path,
    cid: &str,
    zip_data: Option<&TemporaryFile>,
    skill_urls: Vec<String>,
    agent_id: Option<&str>,
    allow_agent_store: bool,
) -> Result<PushedSkills, AppError> {
    push_skills_core_with_store(
        state,
        PushSkillsParams {
            ws,
            cid,
            zip_data,
            skill_urls,
            agent_id,
            allow_agent_store,
            agent_store_root: None,
        },
    )
    .await
}

/// [`push_skills_core`] 的参数结构变体（computer 侧注入 agent-store 根锚定）。
pub async fn push_skills_core_with_store(
    state: &AppState,
    params: PushSkillsParams<'_>,
) -> Result<PushedSkills, AppError> {
    let PushSkillsParams {
        ws,
        cid,
        zip_data,
        skill_urls,
        agent_id,
        allow_agent_store,
        agent_store_root,
    } = params;
    if !crate::service::fs_util::path_exists(ws).await? {
        return Err(AppError::resource("workspace does not exist"));
    }

    // 有 agentId 且 workspace 已软链 → 写 agent-store; 否则旧路径 (对齐 TS pushSkillsToWorkspace)
    let agent_id = agent_id.map(|s| s.trim()).filter(|s| !s.is_empty());
    let updated = if allow_agent_store && let Some(agent_id) = agent_id {
        let skills_path = ws.join(".agents").join("skills");
        if crate::service::agent_store::is_dir_link(&skills_path) {
            let user_root = match agent_store_root {
                Some(root) => root.to_path_buf(),
                None => ws.parent().unwrap_or(ws).to_path_buf(),
            };
            skills_service::push_skills_to_agent_store(skills_service::PushToStoreParams {
                user_root: &user_root,
                cid,
                agent_id,
                zip_path: zip_data.map(|file| file.path()),
                skill_urls,
                downloader: &state.skill_downloader,
            })
            .await?
        } else {
            tracing::info!(
                cid,
                agent_id,
                "push skills: agentId present but workspace not symlinked, use legacy path"
            );
            skills_service::push_skills_at(
                ws,
                zip_data.map(|file| file.path()),
                skill_urls,
                &state.skill_downloader,
            )
            .await?
        }
    } else {
        if let Some(agent_id) = agent_id {
            tracing::info!(
                agent_id,
                "push skills: agent-store path disabled, use legacy path"
            );
        }
        skills_service::push_skills_at(
            ws,
            zip_data.map(|file| file.path()),
            skill_urls,
            &state.skill_downloader,
        )
        .await?
    };
    Ok(PushedSkills { updated })
}

/// push-skills 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn push_skills_impl(
    state: &AppState,
    params: PushSkillsParams<'_>,
) -> Result<Json<Value>, AppError> {
    let ws_display = params.ws.display().to_string();
    let r = push_skills_core_with_store(state, params).await?;
    // message 对齐 nuwax pushSkillsToWorkspace: 有 skills → "Pushed N skills: a, b";
    // 无 → "No valid skill directories found in file or skillUrls"
    let message = if r.updated.is_empty() {
        "No valid skill directories found in file or skillUrls".to_string()
    } else {
        format!(
            "Pushed {} skills: {}",
            r.updated.len(),
            r.updated.join(", ")
        )
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "workspaceRoot": ws_display,
        "updatedSkills": r.updated,
    })))
}

//! skills 推送 + syncAgents (对齐 nuwax `projectService.pushSkillsToWorkspace`
//! + `AgentWorkspaceUtils.syncAgents`)。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::workspace::{ProjectContext, WorkspaceResolver};

pub struct PushResult {
    pub project_path: String,
    pub updated_skills: Vec<String>,
}

/// 推送 skills 到项目 `.claude/skills`, 再 syncAgents 到 `.agents/.opencode/.codex`。
/// 来源: 上传 zip (file) 和/或 skillUrls (HTTP fetch zip)。
pub async fn push_skills(
    resolver: &dyn WorkspaceResolver,
    ctx: &ProjectContext,
    zip_data: Option<Vec<u8>>,
    skill_urls: Vec<String>,
) -> AppResult<PushResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    let updated = push_skills_at(&project_path, zip_data, skill_urls).await?;
    Ok(PushResult {
        project_path: project_path.to_string_lossy().to_string(),
        updated_skills: updated,
    })
}

/// 推送 skills 核心 (path 制): 解压 zip/url → .claude/skills, 再 sync_agents。
/// project 路由 (push_skills) 与 computer 路由共用。
pub async fn push_skills_at(
    workspace: &Path,
    zip_data: Option<Vec<u8>>,
    skill_urls: Vec<String>,
) -> AppResult<Vec<String>> {
    if zip_data.is_none() && skill_urls.is_empty() {
        return Err(AppError::validation(
            "file or skillUrls cannot both be empty",
        ));
    }
    // 权威 agent 目录 = .agents (对齐 nuwax AgentWorkspaceUtils PRIMARY_AGENT_TYPE="agents")
    let primary_skills = workspace.join(".agents").join("skills");
    let primary_agents = workspace.join(".agents").join("agents");
    fs::create_dir_all(&primary_skills).await?;
    fs::create_dir_all(&primary_agents).await?;

    // 收集 zip 来源 (上传 + url fetch)
    let mut sources: Vec<Vec<u8>> = Vec::new();
    if let Some(d) = zip_data {
        sources.push(d);
    }
    for url in skill_urls {
        sources.push(fetch_url(&url).await?);
    }

    let mut updated = Vec::new();
    for (i, data) in sources.iter().enumerate() {
        let extract_root = temp_dir(&format!("fs-skills-{i}"));
        let temp_zip = extract_root.join("src.zip");
        let extract_result: AppResult<()> = async {
            fs::write(&temp_zip, data).await?;
            crate::service::zip::extract_to(temp_zip.clone(), extract_root.clone()).await
        }
        .await;
        if let Err(e) = extract_result {
            let _ = fs::remove_dir_all(&extract_root).await;
            return Err(e);
        }
        // 找 skills 目录 (根 skills/ 或一层子 skills/)
        if let Some(skills_dir) = find_skills_dir(&extract_root) {
            let mut entries = fs::read_dir(&skills_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().to_string();
                let src = entry.path();
                let dst = primary_skills.join(&name);
                let _ = fs::remove_dir_all(&dst).await;
                if copy_entry(&src, &dst).await.is_ok() {
                    updated.push(name);
                }
            }
        }
        let _ = fs::remove_dir_all(&extract_root).await;
    }

    sync_agents(workspace).await?;
    Ok(updated)
}

pub async fn fetch_url(url: &str) -> AppResult<Vec<u8>> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| AppError::network(format!("fetch skill url failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::network(format!(
            "fetch {url} returned status {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::network(format!("read skill url failed: {e}")))?;
    Ok(bytes.to_vec())
}

/// 在 extract_root 下找 skills 目录 (优先根 skills/, 再查一层子目录 skills/)。
fn find_skills_dir(root: &Path) -> Option<PathBuf> {
    let direct = root.join("skills");
    if direct.is_dir() {
        return Some(direct);
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let sub = entry.path().join("skills");
            if sub.is_dir() {
                return Some(sub);
            }
        }
    }
    None
}

/// 以 `.agents` 为权威源, 全量 fan-out skills/agents 到 `.claude/.opencode/.codex`
/// (对齐 nuwax AgentWorkspaceUtils syncAgents; PRIMARY_AGENT_TYPE="agents")。
pub async fn sync_agents(project_path: &Path) -> AppResult<()> {
    let primary_skills = project_path.join(".agents").join("skills");
    let primary_agents = project_path.join(".agents").join("agents");
    for agent_root in [".claude", ".opencode", ".codex"] {
        let t_root = project_path.join(agent_root);
        fs::create_dir_all(&t_root).await?;
        let t_skills = t_root.join("skills");
        let t_agents = t_root.join("agents");
        // skills (先 rm 再 copy, 全量覆盖)
        let _ = fs::remove_dir_all(&t_skills).await;
        fs::create_dir_all(&t_skills).await?;
        if fs::try_exists(&primary_skills).await.unwrap_or(false) {
            crate::service::fs_util::copy_dir_filtered(&primary_skills, &t_skills, &[], &[])
                .await?;
        }
        // agents
        let _ = fs::remove_dir_all(&t_agents).await;
        fs::create_dir_all(&t_agents).await?;
        if fs::try_exists(&primary_agents).await.unwrap_or(false) {
            crate::service::fs_util::copy_dir_filtered(&primary_agents, &t_agents, &[], &[])
                .await?;
        }
    }
    Ok(())
}

async fn copy_entry(src: &Path, dst: &Path) -> AppResult<()> {
    let ft = fs::metadata(src).await?;
    if ft.is_dir() {
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(src, dst).await?;
        Ok(())
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("{prefix}_{nanos}"));
    let _ = std::fs::create_dir_all(&p);
    p
}

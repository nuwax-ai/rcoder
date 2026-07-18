//! skills 推送 + syncAgents (对齐 nuwax `projectService.pushSkillsToWorkspace`
//! + `AgentWorkspaceUtils.syncAgents`)。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::service::skill_download::SkillDownloader;
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
    zip_path: Option<&Path>,
    skill_urls: Vec<String>,
    downloader: &SkillDownloader,
) -> AppResult<PushResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let updated = push_skills_at(&project_path, zip_path, skill_urls, downloader).await?;
    Ok(PushResult {
        project_path: project_path.to_string_lossy().to_string(),
        updated_skills: updated,
    })
}

/// 推送 skills 核心 (path 制): 解压 zip/url → .claude/skills, 再 sync_agents。
/// project 路由 (push_skills) 与 computer 路由共用。
pub async fn push_skills_at(
    workspace: &Path,
    zip_path: Option<&Path>,
    skill_urls: Vec<String>,
    downloader: &SkillDownloader,
) -> AppResult<Vec<String>> {
    if zip_path.is_none() && skill_urls.is_empty() {
        return Err(AppError::validation(
            "file or skillUrls cannot both be empty",
        ));
    }
    // 权威 agent 目录 = .agents (对齐 nuwax AgentWorkspaceUtils PRIMARY_AGENT_TYPE="agents")
    let primary_skills = workspace.join(".agents").join("skills");
    let primary_agents = workspace.join(".agents").join("agents");
    fs::create_dir_all(&primary_skills).await?;
    fs::create_dir_all(&primary_agents).await?;

    let mut updated = Vec::new();
    if let Some(source) = zip_path {
        updated.extend(import_skill_archive(workspace, &primary_skills, source).await?);
    }
    for url in skill_urls {
        let downloaded = downloader.download(&url).await?;
        updated.extend(import_skill_archive(workspace, &primary_skills, downloaded.path()).await?);
    }

    sync_agents(workspace).await?;
    Ok(updated)
}

async fn import_skill_archive(
    workspace: &Path,
    primary_skills: &Path,
    source: &Path,
) -> AppResult<Vec<String>> {
    let parent = workspace.parent().unwrap_or(workspace).to_path_buf();
    let extract_guard = crate::service::temp_file::tempdir_in(parent, ".skills-extract-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    crate::service::zip::extract_to(source.to_path_buf(), extract_root.clone()).await?;
    let mut imported = Vec::new();
    if let Some(skills_dir) = find_skills_dir(&extract_root).await? {
        let mut entries = fs::read_dir(&skills_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let src = entry.path();
            let dst = primary_skills.join(&name);
            remove_dir_if_exists(&dst).await?;
            copy_entry(&src, &dst).await?;
            imported.push(name);
        }
    }
    Ok(imported)
}

/// 在 extract_root 下找 skills 目录 (优先根 skills/, 再查一层子目录 skills/)。
async fn find_skills_dir(root: &Path) -> AppResult<Option<PathBuf>> {
    let direct = root.join("skills");
    if fs::try_exists(&direct).await? {
        return Ok(Some(direct));
    }
    let mut entries = fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let sub = entry.path().join("skills");
        if fs::try_exists(&sub).await? {
            return Ok(Some(sub));
        }
    }
    Ok(None)
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
        if crate::service::fs_util::path_exists(&primary_skills).await? {
            crate::service::fs_util::copy_dir_filtered(&primary_skills, &t_skills, &[], &[])
                .await?;
        }
        // agents
        let _ = fs::remove_dir_all(&t_agents).await;
        fs::create_dir_all(&t_agents).await?;
        if crate::service::fs_util::path_exists(&primary_agents).await? {
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

async fn remove_dir_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_nested_skills_directory_without_blocking_io() {
        let root_guard = tempfile::tempdir().expect("create test directory");
        let root = root_guard.path().to_path_buf();
        let expected = root.join("bundle").join("skills");
        fs::create_dir_all(&expected)
            .await
            .expect("create skills fixture");

        let found = find_skills_dir(&root).await.expect("scan skills directory");
        assert_eq!(found, Some(expected));
    }
}

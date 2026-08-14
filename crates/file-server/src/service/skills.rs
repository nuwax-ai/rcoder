//! skills 推送 + syncAgents (对齐 nuwax `projectService.pushSkillsToWorkspace`
//! + `AgentWorkspaceUtils.syncAgents`)。

use std::path::{Path, PathBuf};

use futures_util::future::try_join_all;
use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::service::skill_download::SkillDownloader;
use crate::workspace::{ProjectContext, WorkspaceResolver};

pub struct PushResult {
    pub project_path: String,
    pub updated_skills: Vec<String>,
}

/// 推送 skills 到权威源 `.agents/skills`, 再 sync_agents fan-out 到五家 ACP 目录。
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

/// push_skills_to_agent_store 参数 (结构化入参, 避免 too_many_arguments)。
pub struct PushToStoreParams<'a> {
    /// 会话工作区父目录 (Local={root}/{userId}, Subvolume=subvolume_base)
    pub user_root: &'a Path,
    pub cid: &'a str,
    pub agent_id: &'a str,
    pub zip_path: Option<&'a Path>,
    pub skill_urls: Vec<String>,
    pub downloader: &'a SkillDownloader,
}

/// push-skills to agent-store (对齐 TS pushSkillsToAgentStore)。
/// 动态技能写入实体目录 (打 `.dynamic_add.lock`), 并确保会话工作区软链。
/// 调用前提: 会话工作区 `.agents/skills` 已是软链 (由 create_workspace_with_agent_store 建立)。
pub async fn push_skills_to_agent_store(params: PushToStoreParams<'_>) -> AppResult<Vec<String>> {
    let PushToStoreParams {
        user_root,
        cid,
        agent_id,
        zip_path,
        skill_urls,
        downloader,
    } = params;
    let session_workspace = user_root.join(cid);
    fs::create_dir_all(&session_workspace).await?;

    let (agent_skills_dir, agent_agents_dir) =
        crate::service::agent_store::ensure_agent_store_dirs(user_root, agent_id).await?;

    let mut updated: Vec<String> = Vec::new();

    // 处理上传 zip
    if let Some(zip) = zip_path {
        updated.extend(
            install_skills_from_zip_dynamic(zip, &agent_skills_dir, &session_workspace).await?,
        );
    }

    // 处理 skill_urls
    for url in &skill_urls {
        let downloaded = downloader.download(url).await?;
        updated.extend(
            install_skills_from_zip_dynamic(
                downloaded.path(),
                &agent_skills_dir,
                &session_workspace,
            )
            .await?,
        );
    }

    // 软链会话工作区 → agent-store (确保链接有效)
    crate::service::agent_store::link_workspace_to_agent_store(
        &session_workspace,
        &agent_skills_dir,
        &agent_agents_dir,
    )
    .await?;

    tracing::info!(
        op = "push_skills_to_agent_store",
        updated_skills = updated.len(),
        "pushed skills to agent store"
    );
    Ok(updated)
}

/// 解压 zip → 安装所有 skill 子目录到 agent-store (as_dynamic = true)。
/// 动态安装的 skill 打 `.dynamic_add.lock`, prune 时保留。
async fn install_skills_from_zip_dynamic(
    zip_path: &Path,
    agent_skills_dir: &Path,
    session_workspace: &Path,
) -> AppResult<Vec<String>> {
    let tmp = session_workspace.join(".tmp");
    fs::create_dir_all(&tmp).await?;
    let extract_guard = crate::service::temp_file::tempdir_in(tmp, ".push-skill-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    crate::service::zip::extract_to(zip_path.to_path_buf(), extract_root.clone()).await?;

    // 候选: 优先 skills/ 子目录, 否则顶层非隐藏目录
    let skills_sub = extract_root.join("skills");
    let base = if skills_sub.is_dir() {
        skills_sub
    } else {
        extract_root
    };
    let mut updated = Vec::new();
    let mut rd = fs::read_dir(&base).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        crate::service::agent_store::install_skill_dir(
            &entry.path(),
            agent_skills_dir,
            &name,
            true,
        )
        .await?;
        updated.push(name);
    }
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

/// sync_agents fan-out 目标目录: 权威源 .agents/{skills,agents} → 各家 ACP agent 约定目录。
/// 加新 agent 在此追加目录名即可 (注意: .agents 是权威源本身, 不在此列)。
/// 源码实证: claude(.claude) / opencode(.opencode) / codex(.codex) / grok(.grok) / pi(.pi)。
pub const SYNC_TARGET_DIRS: &[&str] = &[".claude", ".opencode", ".codex", ".grok", ".pi"];

/// 所有 agent 目录 (含权威源 .agents + 各家 ACP 目录)。
/// `link_workspace_to_agent_store` 用此列表把每个目录的 {skills,agents} 软链到 agent-store。
pub const ALL_AGENT_DIRS: &[&str] = &[".agents", ".claude", ".opencode", ".codex", ".grok", ".pi"];

/// fan-out 版本标识 (SYNC_TARGET_DIRS 派生): sync_agents 写入 `.agents/.sync_version`,
/// 启动 reconciler 据此 O(1) 判断是否需补 sync。加新 agent 改 SYNC_TARGET_DIRS 即自动变版本。
pub fn sync_target_version() -> String {
    SYNC_TARGET_DIRS.join(",")
}

/// 以 `.agents` 为权威源, 全量 fan-out skills/agents 到五家 ACP agent 约定目录
/// (对齐 nuwax AgentWorkspaceUtils syncAgents; PRIMARY_AGENT_TYPE="agents")。
///
/// 10 个目录 (5 agent × {skills, agents}) 并发同步 (`try_join_all`);
/// 优先软链 (零拷贝), 失败 fallback 实体复制。
/// 各 agent 的 hook 配置 (`settings.json` / `hooks.json` / `plugins/` 等) 不受影响。
pub async fn sync_agents(project_path: &Path) -> AppResult<()> {
    let start = std::time::Instant::now();
    let primary_skills = project_path.join(".agents").join("skills");
    let primary_agents = project_path.join(".agents").join("agents");
    let has_skills = crate::service::fs_util::path_exists(&primary_skills).await?;
    let has_agents = crate::service::fs_util::path_exists(&primary_agents).await?;

    // 构建所有 (src, dst, has) 三元组, 创建目标根目录
    let mut copy_targets = Vec::new();
    for agent_root in SYNC_TARGET_DIRS {
        let t_root = project_path.join(agent_root);
        fs::create_dir_all(&t_root).await?;
        copy_targets.push((primary_skills.clone(), t_root.join("skills"), has_skills));
        copy_targets.push((primary_agents.clone(), t_root.join("agents"), has_agents));
    }

    // 并发同步: try_join_all 同时 poll 所有 future, 任一失败立即返回。
    // 内部优先软链, 失败 fallback copy; .await 让出点由 tokio 调度。
    try_join_all(
        copy_targets
            .into_iter()
            .map(|(src, dst, has)| async move { sync_dir(&src, &dst, has).await }),
    )
    .await?;

    tracing::info!(
        op = "sync_agents",
        elapsed_ms = start.elapsed().as_millis(),
        targets = SYNC_TARGET_DIRS.len(),
        "skills sync completed"
    );
    // 版本 marker: 启动 reconciler 据此 O(1) 判断是否需补 sync
    if let Err(e) = fs::write(
        project_path.join(".agents").join(".sync_version"),
        sync_target_version(),
    )
    .await
    {
        tracing::warn!(error = %e, "write sync_version marker failed (best-effort, skipping)");
    }
    Ok(())
}

/// 同步单个目录 (优先软链 → fallback copy)。源不存在时创建空目录。
async fn sync_dir(src: &Path, dst: &Path, has_src: bool) -> AppResult<()> {
    // 删旧 dst (NotFound 安全)
    match fs::remove_dir_all(dst).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if !has_src {
        fs::create_dir_all(dst).await?;
        return Ok(());
    }
    // 优先软链 (对齐 TS forceDirSymlink: .agents → 各家 ACP 目录)
    match crate::service::agent_store::force_dir_symlink(dst, src).await {
        Ok(()) => {
            tracing::debug!(
                src = %src.display(),
                dst = %dst.display(),
                "sync_dir: symlink created"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                error = %e,
                "sync_dir: symlink failed, fallback to copy"
            );
            // 软链失败后 dst 可能被部分清理, 确保 copy 前重新删除
            match fs::remove_dir_all(dst).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    // fallback copy
    crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
    Ok(())
}

async fn copy_entry(src: &Path, dst: &Path) -> AppResult<()> {
    let start = std::time::Instant::now();
    let ft = fs::metadata(src).await?;
    let result = if ft.is_dir() {
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(src, dst).await?;
        Ok(())
    };
    tracing::debug!(
        op = "copy_entry",
        elapsed_ms = start.elapsed().as_millis(),
        src = %src.display(),
        "file copy completed"
    );
    result
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

    #[test]
    fn sync_target_version_reflects_target_dirs() {
        // 版本标识 = SYNC_TARGET_DIRS join; 加新 agent 即变版本 (reconciler 据此判断是否需补 sync)
        let v = sync_target_version();
        assert_eq!(v.split(',').count(), SYNC_TARGET_DIRS.len());
        for d in SYNC_TARGET_DIRS {
            assert!(v.contains(*d), "version missing {d}");
        }
    }

    #[tokio::test]
    async fn sync_dir_links_or_copies_when_src_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("SKILL.md"), "test").await.unwrap();
        let dst = ws.join(".claude").join("skills");

        sync_dir(&src, &dst, true).await.unwrap();

        // Unix: 优先软链; Windows/other: fallback copy (实体目录)
        #[cfg(unix)]
        {
            assert!(dst.is_symlink(), "dst should be a symlink on unix");
        }
        #[cfg(not(unix))]
        {
            assert!(!dst.is_symlink(), "dst should be a real directory");
        }
        // 内容可读 (软链和实体目录都透明)
        let content = fs::read_to_string(dst.join("SKILL.md")).await.unwrap();
        assert_eq!(content, "test");
    }

    #[tokio::test]
    async fn sync_dir_creates_empty_dir_when_src_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join(".claude").join("skills");
        sync_dir(&tmp.path().join(".agents").join("skills"), &dst, false)
            .await
            .unwrap();
        assert!(dst.is_dir(), "dst should be an empty directory");
    }

    #[tokio::test]
    async fn sync_dir_replaces_existing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("new.md"), "new").await.unwrap();

        let dst = ws.join(".claude").join("skills");
        // 先建旧目录 + 旧文件
        fs::create_dir_all(&dst).await.unwrap();
        fs::write(dst.join("old.md"), "old").await.unwrap();

        sync_dir(&src, &dst, true).await.unwrap();

        // 旧文件已删 (软链/copy 后 dst 指向 src, src 中无 old.md)
        assert!(!dst.join("old.md").exists(), "old file should be gone");
        // 新文件可读
        assert_eq!(fs::read_to_string(dst.join("new.md")).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn sync_agents_concurrent_copy_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // 准备 .agents/skills + .agents/agents
        let skills = ws.join(".agents").join("skills");
        let agents = ws.join(".agents").join("agents");
        fs::create_dir_all(&skills).await.unwrap();
        fs::create_dir_all(&agents).await.unwrap();
        fs::write(skills.join("a.md"), "a").await.unwrap();
        fs::write(agents.join("b.md"), "b").await.unwrap();

        sync_agents(ws).await.unwrap();

        // 验证所有 5 个目标目录都有 skills 和 agents
        for dir in SYNC_TARGET_DIRS {
            let s = ws.join(dir).join("skills").join("a.md");
            let a = ws.join(dir).join("agents").join("b.md");
            assert!(s.exists(), "{dir}/skills/a.md missing");
            assert!(a.exists(), "{dir}/agents/b.md missing");
        }
    }
}

//! create-workspace (对齐 nuwax computerUtils.createWorkspace + AgentWorkspaceUtils):
//! `.agents/{skills,agents}` 装配 + `.dynamic_add.lock` 保留 + skill/agent zip/url 合并 + syncAgents。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppResult;

use super::DYNAMIC_ADD_LOCK;
use super::helpers::{find_dir, move_dir};

/// 单个 skill URL 推送失败 (best-effort 语义下收集, 透传给调用方)。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SkillFailure {
    pub url: String,
    pub error: String,
}

pub struct CreateWorkspaceResult {
    pub message: String,
    pub updated_skills: Vec<String>,
    /// best-effort: 推送失败的 skill URL 明细 (空 = 全部成功)。透传给调用方,
    /// 避免 skill 缺失被静默吞掉 (SSRF/HTTPS 校验拒绝、下载/解压失败等)。
    pub failed_skills: Vec<SkillFailure>,
}

/// create-workspace 核心 (对齐 nuwax createWorkspace):
/// 1. ensure `.agents/{skills,agents}`
/// 2. 保留含 `.dynamic_add.lock` 的 skill 子目录
/// 3. rm + 重建 `.agents/{skills,agents}`, 还原保留 skills
/// 4. 写入 agent hook 配置 (claude/codex/opencode mcp/hooks/permissions/hookScripts; best-effort)
/// 5. 若无 file 且无 skillUrls → syncAgents + 早退
/// 6. file: 校验 `.zip` + 解压 + 移动 skills/ 子目录与 agents/ 整目录
/// 7. skillUrls: 逐个下载解压, 集成 skill 目录 (skills/<name> 或顶层 <name>)
/// 8. syncAgents
pub async fn create_workspace(
    workspace: &Path,
    skill_zip: Option<&Path>,
    skill_urls: Vec<String>,
    hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
    downloader: Option<&crate::service::skill_download::SkillDownloader>,
) -> AppResult<CreateWorkspaceResult> {
    let (skills_dir, agents_dir) = ensure_primary_agent_dirs(workspace).await?;

    // 保留含 .dynamic_add.lock 的 skill 子目录 (agents 无此逻辑)
    let preserved = preserve_locked_skills(&skills_dir, workspace).await?;

    // rm + 重建 skills/agents
    remove_dir_all_if_exists(&skills_dir).await?;
    remove_dir_all_if_exists(&agents_dir).await?;
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;

    // 还原保留 skills
    restore_locked_skills(&preserved, &skills_dir).await?;

    // workspace 创建保持 nuwax 的 best-effort 语义，但明确记录 Hook 配置错误。
    if let Err(error) = crate::service::agent_hooks::write_agent_hook_configs(
        workspace,
        hook_config.unwrap_or_default(),
    )
    .await
    {
        tracing::error!(%error, "agent hook configuration failed; continuing workspace creation");
    }

    let mut updated_skills: Vec<String> = Vec::new();
    let mut updated_dirs: Vec<&str> = Vec::new();
    let had_file = skill_zip.is_some();
    let has_urls = !skill_urls.is_empty();

    // 无 file 且无 skillUrls → syncAgents + 早退 (对齐 nuwax)
    if !had_file && !has_urls {
        crate::service::skills::sync_agents(workspace).await?;
        return Ok(CreateWorkspaceResult {
            message: "Workspace created (no uploaded file, no skills and agents)".to_string(),
            updated_skills,
            failed_skills: Vec::new(),
        });
    }

    if let Some(source) = skill_zip {
        // 解压到临时目录 (zip 内找 skills/ + agents/)
        let parent = workspace.parent().unwrap_or(workspace).to_path_buf();
        let extract_guard =
            crate::service::temp_file::tempdir_in(parent, ".skill-extract-").await?;
        let extract_root = extract_guard.path().join("content");
        fs::create_dir_all(&extract_root).await?;
        let extract_res =
            crate::service::zip::extract_to(source.to_path_buf(), extract_root.clone()).await;
        match extract_res {
            Ok(()) => {
                // skills/: 逐子目录移动覆盖
                if let Some(src_skills) = find_dir(&extract_root, "skills").await {
                    if let Ok(mut rd) = fs::read_dir(&src_skills).await {
                        while let Ok(Some(entry)) = rd.next_entry().await {
                            let ft = match entry.file_type().await {
                                Ok(t) => t,
                                Err(_) => continue,
                            };
                            if !ft.is_dir() {
                                continue;
                            }
                            let name = entry.file_name().to_string_lossy().to_string();
                            let dst = skills_dir.join(&name);
                            let _ = fs::remove_dir_all(&dst).await;
                            move_dir(&entry.path(), &dst).await?;
                            updated_skills.push(name);
                        }
                    }
                    updated_dirs.push("skills");
                }
                // agents/: 整目录替换
                if let Some(src_agents) = find_dir(&extract_root, "agents").await {
                    let _ = fs::remove_dir_all(&agents_dir).await;
                    move_dir(&src_agents, &agents_dir).await?;
                    updated_dirs.push("agents");
                }
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    // skillUrls: 逐个下载解压, 集成 skill 目录 (对齐 nuwax createWorkspace skillUrls 循环)。
    // best-effort: 单个失败不中断其余, 但收集明细透传给调用方 (避免 skill 缺失被静默吞掉)。
    let mut failed_skills: Vec<SkillFailure> = Vec::new();
    for url in &skill_urls {
        let downloader = downloader
            .ok_or_else(|| crate::error::AppError::system("skill downloader is not configured"))?;
        match process_skill_url(url, &skills_dir, workspace, downloader).await {
            Ok(names) => {
                if !names.is_empty() {
                    if !updated_dirs.contains(&"skills") {
                        updated_dirs.push("skills");
                    }
                    updated_skills.extend(names);
                }
            }
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "skill url processing failed");
                failed_skills.push(SkillFailure {
                    url: url.to_string(),
                    error: e.to_string(),
                });
            }
        }
    }

    // syncAgents: .agents → .claude/.opencode/.codex/.grok/.pi
    crate::service::skills::sync_agents(workspace).await?;

    let mut message = if updated_dirs.is_empty() {
        "Workspace created successfully (skills and agents directories not found)".to_string()
    } else {
        format!(
            "Workspace created successfully, {} updated",
            updated_dirs.join(" and ")
        )
    };
    // 全部 skill URL 失败: 升级日志 + message 标注 (仍 best-effort 不 fail fast, 保留 nuwax
    // 语义; 但让调用方/日志一眼看到 skill 全军覆没, 不再静默吞掉)。
    if !skill_urls.is_empty() && failed_skills.len() == skill_urls.len() {
        tracing::error!(
            count = failed_skills.len(),
            "all skill URLs failed to process; workspace created but no skills applied"
        );
        message.push_str(&format!(
            "; WARNING: all {} skill URL(s) failed (see failedSkills)",
            failed_skills.len()
        ));
    }

    Ok(CreateWorkspaceResult {
        message,
        updated_skills,
        failed_skills,
    })
}

/// 处理单个 skillUrl: 下载 zip → 解压 → 集成 skill 目录到 skills_dir
/// (对齐 nuwax createWorkspace skillUrls: 若含 skills/ 目录则取其子目录, 否则取顶层非隐藏目录)。
async fn process_skill_url(
    url: &str,
    skills_dir: &Path,
    workspace: &Path,
    downloader: &crate::service::skill_download::SkillDownloader,
) -> AppResult<Vec<String>> {
    let downloaded = downloader.download(url).await?;
    let parent = workspace.parent().unwrap_or(workspace).to_path_buf();
    let extract_guard =
        crate::service::temp_file::tempdir_in(parent, ".skill-url-extract-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    let extract_res =
        crate::service::zip::extract_to(downloaded.path().to_path_buf(), extract_root.clone())
            .await;
    extract_res?;
    // 候选 skill 目录: 优先 skills/ 子目录, 否则顶层非隐藏目录 (对齐 nuwax)
    let skills_sub = extract_root.join("skills");
    let base = if skills_sub.is_dir() {
        skills_sub
    } else {
        extract_root.clone()
    };
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(mut rd) = fs::read_dir(&base).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                candidates.push((name, entry.path()));
            }
        }
    }
    fs::create_dir_all(skills_dir).await?;
    let mut updated = Vec::new();
    for (name, src) in candidates {
        let dst = skills_dir.join(&name);
        let _ = fs::remove_dir_all(&dst).await;
        move_dir(&src, &dst).await?;
        updated.push(name);
    }
    Ok(updated)
}

/// 创建权威 agent 目录 `.agents/{skills,agents}` (对齐 nuwax ensurePrimaryAgentDirs;
/// PRIMARY_AGENT_TYPE="agents")。
async fn ensure_primary_agent_dirs(workspace: &Path) -> AppResult<(PathBuf, PathBuf)> {
    let root = workspace.join(".agents");
    let skills_dir = root.join("skills");
    let agents_dir = root.join("agents");
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;
    Ok((skills_dir, agents_dir))
}

async fn remove_dir_all_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 把含 `.dynamic_add.lock` 的 skill 子目录移到临时区保留 (对齐 nuwax hasDynamicAddLock)。
/// 返回 (临时目录, 保留的 skill 名列表)。
async fn preserve_locked_skills(
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<(Option<tempfile::TempDir>, Vec<String>)> {
    let preserved: Vec<String> = Vec::new();
    if !fs::try_exists(skills_dir).await? {
        return Ok((None, preserved));
    }
    let mut to_preserve: Vec<String> = Vec::new();
    let mut rd = fs::read_dir(skills_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let ft = entry.file_type().await?;
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let lock = skills_dir.join(&name).join(DYNAMIC_ADD_LOCK);
        if fs::try_exists(&lock).await? {
            to_preserve.push(name);
        }
    }
    if to_preserve.is_empty() {
        return Ok((None, Vec::new()));
    }
    let parent = workspace.parent().unwrap_or(workspace).to_path_buf();
    let guard = crate::service::temp_file::tempdir_in(parent, ".preserved-skills-").await?;
    let temp = guard.path();
    for name in &to_preserve {
        move_dir(&skills_dir.join(name), &temp.join(name)).await?;
    }
    Ok((Some(guard), to_preserve))
}

/// 还原保留的 skill 子目录。
async fn restore_locked_skills(
    preserved: &(Option<tempfile::TempDir>, Vec<String>),
    skills_dir: &Path,
) -> AppResult<()> {
    let (guard, names) = preserved;
    let Some(temp) = guard.as_ref().map(tempfile::TempDir::path) else {
        return Ok(());
    };
    if names.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(skills_dir).await?;
    for name in names {
        move_dir(&temp.join(name), &skills_dir.join(name)).await?;
    }
    Ok(())
}

// now_nanos 经 super::helpers 引入 (test 用); 抑制未使用告警 (test 之外不用)。
#[cfg(test)]
mod tests {
    use super::super::helpers::now_nanos;
    use super::*;

    #[tokio::test]
    async fn create_workspace_writes_agents_skills() {
        let tmp = std::env::temp_dir().join(format!("fs_cw_{}", now_nanos()));
        let res = create_workspace(&tmp, None, Vec::new(), None, None)
            .await
            .unwrap();
        assert!(tmp.join(".agents").join("skills").is_dir());
        assert!(tmp.join(".agents").join("agents").is_dir());
        // 无 file → 早退 message
        assert!(res.message.contains("no uploaded file"));
        // syncAgents 镜像目录
        assert!(tmp.join(".claude").join("skills").is_dir());
        assert!(tmp.join(".opencode").join("skills").is_dir());
        assert!(tmp.join(".codex").join("skills").is_dir());
        assert!(tmp.join(".grok").join("skills").is_dir());
        assert!(tmp.join(".pi").join("skills").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }
}

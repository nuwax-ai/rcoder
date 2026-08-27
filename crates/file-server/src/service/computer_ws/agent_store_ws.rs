//! create-workspace with agent-store (对齐 TS createWorkspaceWithAgentStore)。
//! 有 agentId 时走实体存储 + 软链; 与 create_ws.rs (无 agentId 旧路径) 互补。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppResult;

use super::create_ws::CreateWorkspaceResult;
use super::helpers::find_dir;
use crate::models::SkillFailure;

/// create_workspace_with_agent_store 参数 (避免 too_many_arguments)。
pub struct CreateAgentStoreParams<'a> {
    /// 会话工作区的父目录 (user 稳定根; Local=`{root}/{userId}`, Subvolume=subvolume_base)
    pub user_root: &'a Path,
    pub cid: &'a str,
    pub agent_id: &'a str,
    pub skill_zip: Option<&'a Path>,
    pub skill_urls: Vec<String>,
    pub skill_url_map: Option<std::collections::BTreeMap<String, String>>,
    /// 配置技能名全集 (差集删除用); **None = 未传, 不 prune** (防误清空 store)
    pub skill_names: Option<Vec<String>>,
    pub update_skill_names: Option<Vec<String>>,
    pub hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
    pub downloader: Option<&'a crate::service::skill_download::SkillDownloader>,
}

/// create-workspace with agent-store (对齐 TS createWorkspaceWithAgentStore)。
/// 有 agentId 时走实体存储 + 软链; 无 agentId 时走旧路径 `create_workspace`。
///
/// 流程:
/// 1. ensure agent-store `{user_root}/.agent-store/{agent_id}/{skills,agents}`
/// 2. 写 hook 配置到会话工作区
/// 3. 处理 file/skillUrlMap/skillUrls → install_skill_dir 到 agent-store (按需安装)
/// 4. update_agents_dir — 刷新子 agent
/// 5. prune_agent_skills — 按 skill_names 差集清理 (None 跳过)
/// 6. link_workspace_to_agent_store — 软链优先, 失败 fallback copy
pub async fn create_workspace_with_agent_store(
    params: CreateAgentStoreParams<'_>,
) -> AppResult<CreateWorkspaceResult> {
    let CreateAgentStoreParams {
        user_root,
        cid,
        agent_id,
        skill_zip,
        skill_urls,
        skill_url_map,
        skill_names,
        update_skill_names,
        hook_config,
        downloader,
    } = params;
    let start = std::time::Instant::now();

    // 会话工作区
    let session_workspace = user_root.join(cid);
    fs::create_dir_all(&session_workspace).await?;

    // 1. 确保 agent-store 目录
    let (agent_skills_dir, agent_agents_dir) =
        crate::service::agent_store::ensure_agent_store_dirs(user_root, agent_id).await?;

    // 2. 写 hook 配置到会话工作区 (best-effort)
    if let Err(error) = crate::service::agent_hooks::write_agent_hook_configs(
        &session_workspace,
        hook_config.unwrap_or_default(),
    )
    .await
    {
        tracing::error!(%error, "agent hook configuration failed; continuing");
    }

    // selective install: update_skill_names 为 Some 时按需安装 (跳过已存在且不在更新列表的)
    let install_ctx = InstallContext {
        selective: update_skill_names.is_some(),
        update_set: update_skill_names
            .iter()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        agent_skills_dir: &agent_skills_dir,
    };

    let mut updated_skills: Vec<String> = Vec::new();
    let mut skipped_skills: Vec<String> = Vec::new();
    let mut failed_skills: Vec<SkillFailure> = Vec::new();

    // 3a. 处理上传 file → 解压 → install_skill_dir 到 agent-store
    let mut agents_src_dir: Option<PathBuf> = None;
    if let Some(source) = skill_zip {
        let tmp = session_workspace.join(".tmp");
        fs::create_dir_all(&tmp).await?;
        let extract_guard = crate::service::temp_file::tempdir_in(tmp, ".skill-extract-").await?;
        let extract_root = extract_guard.path().join("content");
        fs::create_dir_all(&extract_root).await?;
        crate::service::zip::extract_to(source.to_path_buf(), extract_root.clone()).await?;

        // skills/: 逐子目录 install (按需跳过)
        if let Some(src_skills) = find_dir(&extract_root, "skills").await {
            match fs::read_dir(&src_skills).await {
                Ok(mut rd) => {
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().to_string();
                        if !install_ctx.should_install(&name) {
                            skipped_skills.push(name);
                            continue;
                        }
                        crate::service::agent_store::install_skill_dir(
                            &entry.path(),
                            &agent_skills_dir,
                            &name,
                            false,
                        )
                        .await?;
                        updated_skills.push(name);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "read skills/ dir from uploaded zip failed (skipping)");
                }
            }
        }

        // agents/: 记录源目录 (后续 update_agents_dir 用)
        if let Some(src_agents) = find_dir(&extract_root, "agents").await {
            agents_src_dir = Some(src_agents);
        }
    }

    // 3b. 处理 skill_url_map (优先) 或 skill_urls (回退)
    if let Some(url_map) = skill_url_map.as_ref()
        && !url_map.is_empty()
    {
        let dl = downloader
            .ok_or_else(|| crate::error::AppError::system("skill downloader is not configured"))?;
        for (skill_name, skill_url) in url_map {
            // 下载前跳过 (节省带宽, 对齐 TS)
            if !install_ctx.should_install(skill_name) {
                skipped_skills.push(skill_name.clone());
                tracing::info!(
                    skill_name,
                    "skip download skill zip (already in agent store)"
                );
                continue;
            }
            match download_and_install_skill(
                skill_url,
                skill_name,
                &install_ctx,
                &session_workspace,
                dl,
            )
            .await
            {
                Ok((updated, skipped)) => {
                    updated_skills.extend(updated);
                    skipped_skills.extend(skipped);
                }
                Err(e) => {
                    tracing::warn!(url = %skill_url, error = %e, "skill url processing failed");
                    failed_skills.push(SkillFailure {
                        url: skill_url.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    } else if !skill_urls.is_empty() {
        let dl = downloader
            .ok_or_else(|| crate::error::AppError::system("skill downloader is not configured"))?;
        for url in &skill_urls {
            match download_and_install_skill(url, "", &install_ctx, &session_workspace, dl).await {
                Ok((updated, skipped)) => {
                    updated_skills.extend(updated);
                    skipped_skills.extend(skipped);
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
    }

    // 4. 刷新 agents 子目录 (每次 createWorkspace 覆盖)
    crate::service::agent_store::update_agents_dir(agents_src_dir.as_deref(), &agent_agents_dir)
        .await?;

    // 5. 按 skill_names 差集清理 (保留含 .dynamic_add.lock 的)。
    //    None = 客户端未传 skillNames → 跳过 prune (防止空 keep 集误清整个 store)
    if let Some(skill_names) = skill_names.as_ref() {
        crate::service::agent_store::prune_agent_skills(&agent_skills_dir, skill_names).await?;
    }

    // 6. 软链会话工作区 → agent-store (软链优先, 失败 fallback copy)
    crate::service::agent_store::link_workspace_to_agent_store(
        &session_workspace,
        &agent_skills_dir,
        &agent_agents_dir,
    )
    .await?;

    tracing::info!(
        op = "create_workspace_with_agent_store",
        elapsed_ms = start.elapsed().as_millis(),
        updated_skills = updated_skills.len(),
        skipped_skills = skipped_skills.len(),
        "workspace created with agent store"
    );

    let message = if updated_skills.is_empty()
        && skill_zip.is_none()
        && skill_urls.is_empty()
        && skill_url_map.is_none()
    {
        "Workspace linked to agent store".to_string()
    } else {
        format!(
            "Workspace created successfully, {} skill(s) updated",
            updated_skills.len()
        )
    };

    Ok(CreateWorkspaceResult {
        message,
        updated_skills,
        failed_skills,
    })
}

/// 判断是否应该安装该 skill (对齐 TS shouldInstallSkill)。
/// - 非按需模式 (selective=false) → 始终安装
/// - 在 update_set 中 → 安装
/// - 已存在 → 跳过
fn should_install_skill(
    name: &str,
    selective: bool,
    update_set: &std::collections::HashSet<String>,
    skills_dir: &Path,
) -> bool {
    if name.is_empty() || !selective {
        return true;
    }
    if update_set.contains(name) {
        return true;
    }
    !crate::service::agent_store::agent_skill_exists(skills_dir, name)
}

/// 按需安装上下文 (selective 判定三要素打包, 避免函数长参数列表)。
struct InstallContext<'a> {
    selective: bool,
    update_set: std::collections::HashSet<String>,
    agent_skills_dir: &'a Path,
}

impl InstallContext<'_> {
    fn should_install(&self, name: &str) -> bool {
        should_install_skill(
            name,
            self.selective,
            &self.update_set,
            self.agent_skills_dir,
        )
    }
}

/// 下载 skill zip → 解压 → install_skill_dir 到 agent-store
/// (对齐 TS downloadAndInstallSkillUrl)。
/// `dest_name` 非空时按名安装 (在候选中匹配); 为空时遍历所有候选。
/// selective 模式下安装前检查 should_install_skill, 跳过不需要的 (不覆盖已有 skill)。
/// 返回 (updated, skipped): 已安装的 / 被跳过的 skill 名。
async fn download_and_install_skill(
    url: &str,
    dest_name: &str,
    ctx: &InstallContext<'_>,
    session_workspace: &Path,
    downloader: &crate::service::skill_download::SkillDownloader,
) -> AppResult<(Vec<String>, Vec<String>)> {
    let downloaded = downloader.download(url).await?;
    let tmp = session_workspace.join(".tmp");
    fs::create_dir_all(&tmp).await?;
    let extract_guard = crate::service::temp_file::tempdir_in(tmp, ".skill-url-extract-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    crate::service::zip::extract_to(downloaded.path().to_path_buf(), extract_root.clone()).await?;

    // 候选 skill 目录: 优先 skills/ 子目录, 否则顶层非隐藏目录
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

    let mut updated = Vec::new();
    let mut skipped = Vec::new();
    if !dest_name.is_empty() {
        // 按名安装: 找匹配候选; 若无匹配但只有一个候选, 用它
        let matched = candidates.iter().find(|(n, _)| n == dest_name).or_else(|| {
            if candidates.len() == 1 {
                candidates.first()
            } else {
                None
            }
        });
        if let Some((_, src)) = matched
            && ctx.should_install(dest_name)
        {
            crate::service::agent_store::install_skill_dir(
                src,
                ctx.agent_skills_dir,
                dest_name,
                false,
            )
            .await?;
            updated.push(dest_name.to_string());
        } else if matched.is_some() {
            skipped.push(dest_name.to_string());
        }
    } else {
        // 无名: 遍历候选, 按 selective 跳过不需要的
        for (name, src) in &candidates {
            if ctx.should_install(name) {
                crate::service::agent_store::install_skill_dir(
                    src,
                    ctx.agent_skills_dir,
                    name,
                    false,
                )
                .await?;
                updated.push(name.clone());
            } else {
                skipped.push(name.clone());
            }
        }
    }
    Ok((updated, skipped))
}

#[cfg(test)]
mod tests {
    use super::super::helpers::now_nanos;
    use super::*;

    #[tokio::test]
    async fn create_workspace_with_agent_store_links_session() {
        let tmp = std::env::temp_dir().join(format!("fs_cwas_{}", now_nanos()));
        // 模拟 Local 模式布局: {root}/{user}/{cid}, user_root = {root}/{user}
        let user_root = tmp.join("root").join("u1");
        let cid = "s1";
        let agent_id = "a1";

        let res = create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            cid,
            agent_id,
            skill_zip: None,
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
        })
        .await
        .unwrap();

        // agent-store 已创建 (user_root/.agent-store/{agentId})
        let store = user_root.join(".agent-store").join(agent_id);
        assert!(store.join("skills").is_dir());
        assert!(store.join("agents").is_dir());

        // 会话工作区已创建, 各 agent 目录 skills/agents 存在 (软链或实体)
        let session = user_root.join(cid);
        assert!(session.is_dir());
        for dir in crate::service::skills::ALL_AGENT_DIRS {
            assert!(
                session.join(dir).join("skills").exists(),
                "{dir}/skills should exist"
            );
            assert!(
                session.join(dir).join("agents").exists(),
                "{dir}/agents should exist"
            );
        }

        // 无 file/urls → link message
        assert!(res.message.contains("linked") || res.message.contains("success"));
        drop(fs::remove_dir_all(&tmp).await);
    }

    #[tokio::test]
    async fn prune_skipped_when_skill_names_none() {
        // skillNames 未传 (None) → 不 prune: store 中已有 skill 应保留
        let tmp = std::env::temp_dir().join(format!("fs_prune_{}", now_nanos()));
        let user_root = tmp.join("root").join("u1");

        // 预置 store skill
        let (skills_dir, _) =
            crate::service::agent_store::ensure_agent_store_dirs(&user_root, "a1")
                .await
                .unwrap();
        fs::create_dir_all(skills_dir.join("existing-skill"))
            .await
            .unwrap();
        fs::write(skills_dir.join("existing-skill/SKILL.md"), "keep")
            .await
            .unwrap();

        create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            cid: "s1",
            agent_id: "a1",
            skill_zip: None,
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
        })
        .await
        .unwrap();

        assert!(
            skills_dir.join("existing-skill/SKILL.md").exists(),
            "skillNames None must not prune existing skills"
        );
        drop(fs::remove_dir_all(&tmp).await);
    }
}

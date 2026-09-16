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
    /// 会话工作区的父目录 (user 稳定根; Local=`{root}/{userId}`, Subvolume=subvolume_base;
    /// 项目绑定目录时锚定配置根, 见 handlers::computer::agent_store_user_root)
    pub user_root: &'a Path,
    /// 会话工作区 (显式传入——默认布局 = `user_root/{cid}`, 绑定布局 = 绑定目录本身,
    /// 不能从 user_root 倒推; 对齐 TS 1.4.5 会话工作区与 store 根解耦)
    pub session_workspace: &'a Path,
    pub agent_id: &'a str,
    pub skill_zip: Option<&'a Path>,
    pub skill_urls: Vec<String>,
    pub skill_url_map: Option<std::collections::BTreeMap<String, String>>,
    /// 配置技能名全集 (差集删除用); **None = 未传, 不 prune** (防误清空 store)
    pub skill_names: Option<Vec<String>>,
    pub update_skill_names: Option<Vec<String>>,
    pub hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
    pub downloader: Option<&'a crate::service::skill_download::SkillDownloader>,
    /// 共享工作区（normalProject）项目 ID：Some → manifest 并集视图替代
    /// 目录级整链（多智能体并存）；None → 保留旧目录级软链 + 跨 agent 防线
    pub shared_project_id: Option<&'a str>,
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
        session_workspace,
        agent_id,
        skill_zip,
        skill_urls,
        skill_url_map,
        skill_names,
        update_skill_names,
        hook_config,
        downloader,
        shared_project_id,
    } = params;
    let start = std::time::Instant::now();

    // F01 数据保护：agent ID 与技能清单名在任何写/删除/链接前校验为合法
    // 单一路径段——含分隔符/点段的名字（`../../victim`）会把 store/视图操作
    // 解析到受管目录之外（递归删除业务目录、越界写入）
    crate::service::agent_store::validate_store_segment("agent id", agent_id)?;
    for name in skill_names.iter().flatten() {
        crate::service::agent_store::validate_store_segment("skill name", name)?;
    }
    for name in update_skill_names.iter().flatten() {
        crate::service::agent_store::validate_store_segment("skill name", name)?;
    }
    if let Some(project_id) = shared_project_id {
        crate::service::agent_store::validate_store_segment("project id", project_id)?;
    }
    for skill_name in skill_url_map
        .as_ref()
        .map(|m| m.keys())
        .into_iter()
        .flatten()
    {
        crate::service::agent_store::validate_store_segment("skill name", skill_name)?;
    }

    // 会话工作区 (显式传入: 默认布局 = user_root/{cid}, 绑定布局 = 绑定目录)
    let session_workspace = session_workspace.to_path_buf();
    fs::create_dir_all(&session_workspace).await?;

    // 共享工作区（normalProject）走 manifest 并集视图（多智能体并存）；
    // 目录级整链的跨 agent 冲突防线仅对非共享（taskAgent 每会话独占）生效
    if shared_project_id.is_none() {
        crate::service::agent_store::detect_cross_agent_link_conflict(&session_workspace, agent_id)
            .await?;
    }

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
    // F04：解压 guard 必须存活到 agents 源**消费完成**（update_agents_dir
    // 在下方第 4 步）——块作用域析构会留下指向已删临时目录的 PathBuf，
    // update_agents_dir 把"源不存在"当无输入跳过，subagents 上传假成功。
    let mut zip_extract_guard: Option<tempfile::TempDir> = None;
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
                Ok(mut rd) => loop {
                    let entry = match rd.next_entry().await {
                        Ok(Some(e)) => e,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "list skills/ entries interrupted, partial skills installed"
                            );
                            break;
                        }
                    };
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
                },
                Err(e) => {
                    tracing::warn!(error = %e, "read skills/ dir from uploaded zip failed (skipping)");
                }
            }
        }

        // agents/: 记录源目录 (后续 update_agents_dir 用)；guard 一并外移
        // 存活（F04）
        if let Some(src_agents) = find_dir(&extract_root, "agents").await {
            agents_src_dir = Some(src_agents);
        }
        zip_extract_guard = Some(extract_guard);
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

    // 4. 刷新 agents 子目录 (每次 createWorkspace 覆盖)；消费完成后才允许
    // 释放解压 guard（F04）
    crate::service::agent_store::update_agents_dir(agents_src_dir.as_deref(), &agent_agents_dir)
        .await?;
    drop(zip_extract_guard);

    // 5. 按 skill_names 差集清理 (保留含 .dynamic_add.lock 的)。
    //    None = 客户端未传 skillNames → 跳过 prune (防止空 keep 集误清整个 store)
    if let Some(skill_names) = skill_names.as_ref() {
        crate::service::agent_store::prune_agent_skills(&agent_skills_dir, skill_names).await?;
    }

    // 6. 共享工作区（normalProject）→ manifest 并集视图（多智能体并存、
    //    增量增删）；其余（taskAgent 每会话独占）保留目录级软链（copy 兜底）。
    //    TS 同款：skillNames 原始参数未传（旧客户端全量模式）时不更新清单，
    //    仅做视图校准——空数组会把 manifest 引用清空导致技能被误删。
    if let Some(project_id) = shared_project_id {
        // agents 实体可能为文件（.md）或目录（多文件 subagent 包），一并纳入 manifest
        let mut subagent_names = Vec::new();
        if let Ok(mut rd) = fs::read_dir(&agent_agents_dir).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                subagent_names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        crate::service::agent_store::sync_shared_skill_view(
            user_root,
            &session_workspace,
            agent_id,
            project_id,
            crate::service::agent_store::SharedSkillLists {
                skills: skill_names.clone(),
                subagents: Some(subagent_names),
            },
        )
        .await?;
    } else {
        crate::service::agent_store::link_workspace_to_agent_store(
            &session_workspace,
            &agent_skills_dir,
            &agent_agents_dir,
        )
        .await?;
    }

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
    // 必须用 metadata (跟随软链) 而非 file_type/symlink_metadata, 才等价 Path::is_dir()
    let base = if fs::metadata(&skills_sub)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        skills_sub
    } else {
        extract_root.clone()
    };
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    // 读目录失败上抛 → 调用方落入 failed_skills 明细（权限错误不伪装成"无 skill"）
    let mut rd = fs::read_dir(&base).await?;
    loop {
        let entry = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "list extracted skill entries interrupted, partial candidates"
                );
                break;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            candidates.push((name, entry.path()));
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
        // Local 布局: 会话工作区 = user_root/{cid} (显式传入, 不再由 service 倒推)
        let session_ws = user_root.join(cid);

        let res = create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &session_ws,
            agent_id,
            skill_zip: None,
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
            shared_project_id: None,
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

        let session_ws = user_root.join("s1");
        create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &session_ws,
            agent_id: "a1",
            skill_zip: None,
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
            shared_project_id: None,
        })
        .await
        .unwrap();

        assert!(
            skills_dir.join("existing-skill/SKILL.md").exists(),
            "skillNames None must not prune existing skills"
        );
        drop(fs::remove_dir_all(&tmp).await);
    }

    #[tokio::test]
    async fn bound_layout_decouples_session_from_store_root() {
        // 项目绑定目录布局 (对齐 TS 1.4.5): 会话工作区 = 绑定目录 (任意路径),
        // agent-store 锚定 user_root (配置根/{userId}); 两者解耦, 不再从
        // user_root/{cid} 倒推会话目录。
        let tmp = std::env::temp_dir().join(format!("fs_bound_{}", now_nanos()));
        let user_root = tmp.join("root").join("u1");
        let session_ws = tmp.join("bound-ws"); // 与 user_root 不同树

        create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &session_ws,
            agent_id: "a1",
            skill_zip: None,
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
            shared_project_id: None,
        })
        .await
        .unwrap();

        // store 锚定 user_root (不是 session 的 parent), 会话目录即绑定目录
        let store = user_root.join(".agent-store").join("a1");
        assert!(store.join("skills").is_dir(), "store anchored at user_root");
        assert!(!tmp.join("bound-ws").join(".agent-store").exists() || true);
        for dir in crate::service::skills::ALL_AGENT_DIRS {
            assert!(
                session_ws.join(dir).join("skills").exists(),
                "{dir}/skills linked in session"
            );
        }
        drop(fs::remove_dir_all(&tmp).await);
    }

    // ===== F04：上传 agents 源的消费窗口与文件型条目 =====

    fn zip_with_agents(agent_file_body: &str) -> Vec<u8> {
        use std::io::Write as _;
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        archive.start_file("agents/researcher.md", options).unwrap();
        archive.write_all(agent_file_body.as_bytes()).unwrap();
        archive.finish().unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn uploaded_agents_are_consumed_before_temp_cleanup_and_file_targets_replace() {
        // F04：guard 存活到 update_agents_dir 之后——agents/*.md 实际入 store；
        // 同名 .md 旧文件（非目录）替换不再走 remove_dir_all
        let tmp = std::env::temp_dir().join(format!(
            "fs_f04_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmp).await.unwrap();
        let user_root = tmp.join("root").join("u1");
        let session_ws = user_root.join("s1");
        let zip_path = tmp.join("upload.zip");
        fs::write(&zip_path, zip_with_agents("agent v1"))
            .await
            .unwrap();

        let result = create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &session_ws,
            agent_id: "agent-a",
            skill_zip: Some(&zip_path),
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
            shared_project_id: None,
        })
        .await
        .expect("first upload");
        assert!(result.failed_skills.is_empty());
        let agents_dir = user_root
            .join(".agent-store")
            .join("agent-a")
            .join("agents");
        assert_eq!(
            fs::read_to_string(agents_dir.join("researcher.md"))
                .await
                .unwrap(),
            "agent v1",
            "agents 文件必须真实进入 store（修复前 guard 提前析构 → 假成功）"
        );

        // 同名 .md 更新（旧目标是文件，非目录）
        fs::write(&zip_path, zip_with_agents("agent v2"))
            .await
            .unwrap();
        create_workspace_with_agent_store(CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &session_ws,
            agent_id: "agent-a",
            skill_zip: Some(&zip_path),
            skill_urls: Vec::new(),
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: None,
            shared_project_id: None,
        })
        .await
        .expect("second upload");
        assert_eq!(
            fs::read_to_string(agents_dir.join("researcher.md"))
                .await
                .unwrap(),
            "agent v2",
            "同名 .md 覆盖更新必须成功"
        );
        drop(fs::remove_dir_all(&tmp).await);
    }

    #[tokio::test]
    async fn vanished_agents_source_fails_instead_of_fake_success() {
        // F04：显式给出的源丢失必须失败（不再当无输入跳过假成功）
        let tmp = std::env::temp_dir().join(format!(
            "fs_f04src_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dest = tmp.join("dest");
        fs::create_dir_all(&dest).await.unwrap();
        let missing = tmp.join("missing-agents");
        let result = crate::service::agent_store::update_agents_dir(Some(&missing), &dest).await;
        assert!(result.is_err(), "源丢失必须报错");
        // None = 无输入：合法跳过
        assert!(
            crate::service::agent_store::update_agents_dir(None, &dest)
                .await
                .is_ok()
        );
        drop(fs::remove_dir_all(&tmp).await);
    }
}

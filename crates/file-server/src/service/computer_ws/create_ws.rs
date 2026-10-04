//! create-workspace (对齐 nuwax computerUtils.createWorkspace + AgentWorkspaceUtils):
//! `.agents/{skills,agents}` 装配 + `.dynamic_add.lock` 保留 + skill/agent zip/url 合并 + syncAgents。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppResult;
use crate::models::SkillFailure;

use super::DYNAMIC_ADD_LOCK;
use super::helpers::{find_dir, move_dir};

pub struct CreateWorkspaceResult {
    pub message: String,
    pub updated_skills: Vec<String>,
    /// best-effort: 推送失败的 skill URL 明细 (空 = 全部成功)。透传给调用方,
    /// 避免 skill 缺失被静默吞掉 (SSRF/HTTPS 校验拒绝、下载/解压失败等)。
    pub failed_skills: Vec<SkillFailure>,
    /// Agent Store 路径及状态仅在 v2 Agent Store 模式下返回。
    pub agent_store_path: Option<String>,
    pub skipped_skills: Option<Vec<String>>,
    pub skipped_store_update: Option<bool>,
}

/// create-workspace 核心 (对齐 nuwax createWorkspace):
/// 1. ensure `.agents/{skills,agents}`
/// 2. 保留含 `.dynamic_add.lock` 的 skill 子目录
/// 3. rm + 重建 `.agents/{skills,agents}`, 还原保留 skills
/// 4. 写入 agent hook 配置 (claude/codex/opencode mcp/hooks/permissions/hookScripts; best-effort)
/// 5. 若无 file 且无 skillUrls → syncAgents + 早退
/// 6. file: 校验 `.zip` + 解压 + 移动 skills/ 子目录与 agents/ 整目录
/// 7. skillUrls: 逐个下载解压, 集成 skill 目录 (`skills/<name>` 或顶层 `<name>`)
/// 8. syncAgents
pub async fn create_workspace(
    workspace: &Path,
    skill_zip: Option<&Path>,
    skill_urls: Vec<String>,
    hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
    downloader: Option<&crate::service::skill_download::SkillDownloader>,
) -> AppResult<CreateWorkspaceResult> {
    let start = std::time::Instant::now();
    let (skills_dir, agents_dir) = ensure_primary_agent_dirs(workspace).await?;

    // 保留含 .dynamic_add.lock 的 skill 子目录 (agents 无此逻辑)
    let preserved = preserve_locked_skills(&skills_dir, workspace).await?;

    // rm + 重建 skills/agents
    remove_dir_all_if_exists(&skills_dir).await?;
    remove_dir_all_if_exists(&agents_dir).await?;
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;

    // 还原保留 skills; 失败时唯一副本留在保留区（见 preserve/restore 的诊断路径）
    restore_locked_skills(&preserved, &skills_dir).await?;

    // 全部恢复确认后清理保留区; 清理失败只告警（技能已安全, 残留目录无害）
    if let Some(preserve) = preserved.0
        && let Err(error) = preserve.confirmed_cleanup().await
    {
        tracing::warn!(%error, "cleaning confirmed preserve directory failed (skills already restored)");
    }

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
        tracing::info!(
            op = "create_workspace",
            elapsed_ms = start.elapsed().as_millis(),
            "workspace creation completed (no file, no urls)"
        );
        return Ok(CreateWorkspaceResult {
            message: "Workspace created (no uploaded file, no skills and agents)".to_string(),
            updated_skills,
            failed_skills: Vec::new(),
            agent_store_path: None,
            skipped_skills: None,
            skipped_store_update: None,
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
                // skills/: 逐子目录移动覆盖 (读目录/遍历失败要留痕, 不能静默吞掉整包 skill)
                if let Some(src_skills) = find_dir(&extract_root, "skills").await {
                    match fs::read_dir(&src_skills).await {
                        Ok(mut rd) => {
                            loop {
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
                                let ft = match entry.file_type().await {
                                    Ok(t) => t,
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            entry = %entry.file_name().to_string_lossy(),
                                            "skill entry file_type failed (skipped)"
                                        );
                                        continue;
                                    }
                                };
                                if !ft.is_dir() {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().to_string();
                                let dst = skills_dir.join(&name);
                                if let Err(e) = fs::remove_dir_all(&dst).await
                                    && e.kind() != std::io::ErrorKind::NotFound
                                {
                                    tracing::warn!(error = %e, "clear existing skill dir before move failed");
                                }
                                move_dir(&entry.path(), &dst).await?;
                                updated_skills.push(name);
                            }
                            updated_dirs.push("skills");
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "read skills/ dir from uploaded zip failed (skipping)"
                            );
                        }
                    }
                }
                // agents/: 整目录替换
                if let Some(src_agents) = find_dir(&extract_root, "agents").await {
                    if let Err(e) = fs::remove_dir_all(&agents_dir).await {
                        tracing::warn!(error = %e, "clear agents dir before move failed (skipping)");
                    }
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

    // syncAgents: .agents → SYNC_TARGET_DIRS 各家 ACP 目录 (grok/pi 临时屏蔽)
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

    tracing::info!(
        op = "create_workspace",
        elapsed_ms = start.elapsed().as_millis(),
        updated_skills = updated_skills.len(),
        "workspace creation completed"
    );

    Ok(CreateWorkspaceResult {
        message,
        updated_skills,
        failed_skills,
        agent_store_path: None,
        skipped_skills: None,
        skipped_store_update: None,
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
    // 读目录失败上抛 → 调用方落入 failed_skills 明细; 权限错误不再伪装成"无 skill"
    let mut rd = fs::read_dir(&base).await?;
    loop {
        let entry = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "list extracted skill entries interrupted, partial candidates");
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
    fs::create_dir_all(skills_dir).await?;
    let mut updated = Vec::new();
    for (name, src) in candidates {
        let dst = skills_dir.join(&name);
        if let Err(e) = fs::remove_dir_all(&dst).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "clear existing skill dir before move failed");
        }
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

/// 保留区句柄: 恢复**确认**前不自动清理 (FS-05)。
/// 没有 Drop 删除——guard 被 drop（上层错误/取消）时目录与内容原地保留,
/// 路径已写入诊断日志; 只有 [`PreservedSkills::confirmed_cleanup`] 在全部
/// 技能成功回到 skills/ 后显式移除保留区。
pub(crate) struct PreservedSkills {
    dir: PathBuf,
}

impl PreservedSkills {
    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// 全部恢复确认后调用; 清理失败只告警（数据已安全, 残留目录无害）。
    pub(crate) async fn confirmed_cleanup(self) -> AppResult<()> {
        fs::remove_dir_all(&self.dir).await.map_err(|error| {
            crate::error::AppError::system(format!(
                "remove preserve directory {}: {error}",
                self.dir.display()
            ))
        })
    }
}

/// 把含 `.dynamic_add.lock` 的 skill 子目录移到保留区 (对齐 nuwax hasDynamicAddLock)。
/// 返回 (保留区句柄, 保留的 skill 名列表)。保留区是普通目录而非 TempDir:
/// 中途失败/上层错误/取消时已移入的唯一副本原地保留 (FS-05)。
async fn preserve_locked_skills(
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<(Option<PreservedSkills>, Vec<String>)> {
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
    let dir = parent.join(format!(
        ".preserved-skills-{}",
        uuid::Uuid::now_v7().simple()
    ));
    fs::create_dir_all(&dir).await?;
    for name in &to_preserve {
        if let Err(error) = move_dir(&skills_dir.join(name), &dir.join(name)).await {
            tracing::error!(
                %error,
                preserve_dir = %dir.display(),
                skill = %name,
                "preserving locked skill failed; already-moved copies stay in the preserve directory"
            );
            return Err(crate::error::AppError::system(format!(
                "preserve locked skill '{name}' failed: {error}; moved copies stay at {}",
                dir.display()
            )));
        }
    }
    Ok((Some(PreservedSkills { dir }), to_preserve))
}

/// 还原保留的 skill 子目录。失败时数据留在保留区（不清理）, 错误携带保留区路径。
async fn restore_locked_skills(
    preserved: &(Option<PreservedSkills>, Vec<String>),
    skills_dir: &Path,
) -> AppResult<()> {
    let (holder, names) = preserved;
    let Some(preserve) = holder.as_ref() else {
        return Ok(());
    };
    if names.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(skills_dir).await?;
    for name in names {
        move_dir(&preserve.path().join(name), &skills_dir.join(name))
            .await
            .map_err(|error| {
                tracing::error!(
                    %error,
                    preserve_dir = %preserve.path().display(),
                    skill = %name,
                    "restoring locked skill failed; data stays in the preserve directory"
                );
                crate::error::AppError::system(format!(
                    "restore locked skill '{name}' failed: {error}; preserved copy stays at {}",
                    preserve.path().display()
                ))
            })?;
    }
    Ok(())
}

// now_nanos 经 super::helpers 引入 (test 用); 抑制未使用告警 (test 之外不用)。
#[cfg(test)]
mod tests {
    use super::super::helpers::now_nanos;
    use super::*;

    /// FS-05 反例: restore 失败（或上层错误导致保留区 guard drop）时, 被 move
    /// 进临时区的技能是唯一副本, 不得被自动清理。修复前 TempDir drop 删除数据。
    #[tokio::test]
    async fn locked_skills_survive_failed_restore() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        let skills_dir = workspace.join(".agents").join("skills");
        fs::create_dir_all(&skills_dir).await.expect("skills dir");
        for name in ["skill-a", "skill-b"] {
            let dir = skills_dir.join(name);
            fs::create_dir_all(&dir).await.expect("skill dir");
            fs::write(dir.join(DYNAMIC_ADD_LOCK), b"")
                .await
                .expect("lock");
            fs::write(dir.join("SKILL.md"), format!("# {name}"))
                .await
                .expect("content");
        }

        let preserved = preserve_locked_skills(&skills_dir, &workspace)
            .await
            .expect("preserve");
        let (Some(guard), names) = (&preserved.0, &preserved.1) else {
            panic!("two locked skills must be preserved");
        };
        assert_eq!(names.len(), 2);
        let preserve_dir = guard.path().to_path_buf();
        assert!(preserve_dir.join("skill-a/SKILL.md").is_file());

        // 破坏 restore: skill-a 的目标位置被普通文件占用 → move_dir 失败
        // （preserve 已把源移走, 直接在空位放占位文件）
        fs::write(skills_dir.join("skill-a"), b"placeholder")
            .await
            .expect("placeholder");
        let result = restore_locked_skills(&preserved, &skills_dir).await;
        assert!(result.is_err(), "restore must fail on occupied target");

        // 模拟上层错误路径: 保留区 guard 被 drop
        drop(preserved);
        assert!(
            preserve_dir.join("skill-b/SKILL.md").is_file(),
            "the only surviving copy must stay in the preserve directory"
        );
        assert!(
            preserve_dir.join("skill-a/SKILL.md").is_file(),
            "already-moved skill must stay in the preserve directory"
        );
        drop(fs::remove_dir_all(&preserve_dir).await);
    }

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
        // syncAgents 镜像目录 (grok/pi 临时屏蔽, 不再创建)
        assert!(tmp.join(".claude").join("skills").is_dir());
        assert!(tmp.join(".opencode").join("skills").is_dir());
        assert!(tmp.join(".codex").join("skills").is_dir());
        assert!(!tmp.join(".grok").join("skills").exists());
        assert!(!tmp.join(".pi").join("skills").exists());
        // sync_agents 写版本 marker (启动 reconciler 据此 O(1) 判断是否需补 sync)
        assert!(tmp.join(".agents").join(".sync_version").is_file());
        drop(fs::remove_dir_all(&tmp).await);
    }
}

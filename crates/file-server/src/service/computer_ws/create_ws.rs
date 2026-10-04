//! create-workspace (对齐 nuwax computerUtils.createWorkspace + AgentWorkspaceUtils):
//! `.agents/{skills,agents}` 装配 + `.dynamic_add.lock` 保留 + skill/agent zip/url 合并 + syncAgents。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};
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
    // P1-2: 同一工作区的 preserve/restore/resume 串行化（进程内; 跨进程多写者
    // 如实声明不支持——保留区回执+幂等续行保证数据不丢, 但并发窗口内另一方
    // 可能观察到中间态并报错）。
    let _workspace_guard = workspace_preservation_lock(workspace).await;
    // P1-2: 先续行上次未完成的保留恢复（精确回执驱动, 不扫描随机目录）。
    // 重建进程/取消后的重试从此处闭环: 部分恢复幂等继续, 未确认不虚报完成。
    resume_unfinished_preservation(workspace).await?;

    let (skills_dir, agents_dir) = ensure_primary_agent_dirs(workspace).await?;

    // 保留含 .dynamic_add.lock 的 skill 子目录 (agents 无此逻辑)
    let preserved = preserve_locked_skills(&skills_dir, workspace).await?;

    // rm + 重建 skills/agents
    remove_dir_all_if_exists(&skills_dir).await?;
    remove_dir_all_if_exists(&agents_dir).await?;
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;

    // 还原保留 skills（幂等; 失败时唯一副本留在保留区, 重试入口按回执续行）
    restore_locked_skills(&preserved, workspace).await?;

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
        let downloader =
            downloader.ok_or_else(|| AppError::system("skill downloader is not configured"))?;
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

/// P1-2: 保留区位于 workspace 持久卷内（`.agents/.preserved-skills/`——
/// `create_workspace` 的清理范围只有 `.agents/skills` 与 `.agents/agents`,
/// 保留区不受影响; 也不再放 workspace 父目录——挂载布局下父目录不保证持久）。
fn preserve_area(workspace: &Path) -> PathBuf {
    workspace.join(".agents").join(".preserved-skills")
}

fn preserve_receipt_path(workspace: &Path) -> PathBuf {
    preserve_area(workspace).join("receipt.json")
}

/// 保留操作的持久回执: 中断后按此精确续行, 不按随机目录名猜归属。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreserveReceipt {
    version: u32,
    operation_id: String,
    /// 恢复归属核验: 只有同一工作区（canonical 路径）认领自己的保留区。
    workspace_root: String,
    /// 待恢复的 skill 名单（保留完成时固化; 恢复按名单幂等推进）。
    skills: Vec<String>,
}

const PRESERVE_RECEIPT_VERSION: u32 = 1;

/// 进程内 per-workspace 互斥（P1-2 竞争边界）。
async fn workspace_preservation_lock(workspace: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<
            std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = workspace.to_path_buf();
    let mutex = {
        let mut locks = LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(key).or_default().clone()
    };
    mutex.lock_owned().await
}

/// P1-2: 续行上次未完成的保留恢复。幂等: 名单项在保留区 → 搬回; 名单项已在
/// skills/（先前部分恢复）→ 权威位已有, 清除保留区残留副本; 名单项两处都无
/// （preserve 中途失败未移走）→ 原位即权威, 视为已恢复。全部确认后清理回执
/// 与保留区; 任何失败保留回执与剩余副本, 错误携带阶段与恢复来源路径。
async fn resume_unfinished_preservation(workspace: &Path) -> AppResult<()> {
    let receipt_path = preserve_receipt_path(workspace);
    let Some(raw) = read_optional_text(&receipt_path).await? else {
        return Ok(());
    };
    let receipt: PreserveReceipt = serde_json::from_str(&raw).map_err(|error| {
        AppError::system(format!(
            "invalid preserve receipt {}: {error} (receipt preserved, manual recovery may be required)",
            receipt_path.display()
        ))
    })?;
    if receipt.version != PRESERVE_RECEIPT_VERSION {
        return Err(AppError::system(format!(
            "unsupported preserve receipt version {} at {} (receipt preserved)",
            receipt.version,
            receipt_path.display()
        )));
    }
    let canonical = fs::canonicalize(workspace)
        .await
        .map_err(|error| AppError::system(format!("resolve {}: {error}", workspace.display())))?;
    if receipt.workspace_root != canonical.to_string_lossy() {
        return Err(AppError::system(format!(
            "preserve receipt at {} belongs to workspace {}, not this workspace ({}); refusing to claim foreign copies",
            receipt_path.display(),
            receipt.workspace_root,
            canonical.display()
        )));
    }
    restore_from_receipt(workspace, &receipt).await?;
    confirm_preservation_finished(workspace, &receipt).await
}

/// 按回执名单逐项恢复（幂等推进）。失败时回执与剩余副本保留。
async fn restore_from_receipt(workspace: &Path, receipt: &PreserveReceipt) -> AppResult<()> {
    let area = preserve_area(workspace);
    let skills_dir = workspace.join(".agents").join("skills");
    fs::create_dir_all(&skills_dir).await?;
    for name in &receipt.skills {
        let source = area.join(name);
        let target = skills_dir.join(name);
        match fs::try_exists(&source).await {
            Ok(true) => {
                match fs::try_exists(&target).await? {
                    false => {
                        move_dir(&source, &target).await.map_err(|error| {
                            AppError::system(format!(
                                "resume locked skill '{name}' failed: {error}; remaining copies stay at {}",
                                area.display()
                            ))
                        })?;
                    }
                    true if target.is_dir() => {
                        // 权威位已有先前部分恢复的内容: 不覆盖, 清除保留区残留副本。
                        remove_dir_all_if_exists(&source).await?;
                    }
                    // 目标被非目录条目占用（冲突）: 不是权威内容, 保留区副本
                    // 不得删除——报告冲突, 清障后重试幂等续行。
                    true => {
                        return Err(AppError::system(format!(
                            "target of locked skill '{name}' is occupied by a non-directory entry ({}); clear the conflict and retry; preserved copy stays at {}",
                            target.display(),
                            source.display()
                        )));
                    }
                }
            }
            Ok(false) => {
                // 保留区无副本: preserve 中途失败未移走, 原位即权威。
            }
            Err(error) => {
                return Err(AppError::system(format!(
                    "inspect preserved skill '{name}' at {}: {error}",
                    source.display()
                )));
            }
        }
    }
    Ok(())
}

/// 全部恢复确认后: 删除回执与保留区（清理失败只告警——技能已安全）。
async fn confirm_preservation_finished(
    workspace: &Path,
    receipt: &PreserveReceipt,
) -> AppResult<()> {
    let _ = receipt;
    let receipt_path = preserve_receipt_path(workspace);
    fs::remove_file(&receipt_path).await.map_err(|error| {
        AppError::system(format!(
            "remove confirmed preserve receipt {}: {error}",
            receipt_path.display()
        ))
    })?;
    if let Err(error) = fs::remove_dir_all(preserve_area(workspace)).await {
        tracing::warn!(%error, "cleaning confirmed preserve area failed (skills already restored)");
    }
    Ok(())
}

/// 把含 `.dynamic_add.lock` 的 skill 子目录移到保留区 (对齐 nuwax hasDynamicAddLock)。
/// P1-2: 保留区在 workspace 持久卷内, 先持久化回执（名单+身份）再逐项移动——
/// 中途失败/取消/进程重建后, 入口的 `resume_unfinished_preservation` 按回执
/// 精确续行; 已移入的唯一副本不删除 (FS-05)。
async fn preserve_locked_skills(
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<(Option<PreserveAreaHandle>, Vec<String>)> {
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
    let area = preserve_area(workspace);
    fs::create_dir_all(&area).await?;
    let canonical = fs::canonicalize(workspace)
        .await
        .map_err(|error| AppError::system(format!("resolve {}: {error}", workspace.display())))?;
    let receipt = PreserveReceipt {
        version: PRESERVE_RECEIPT_VERSION,
        operation_id: uuid::Uuid::now_v7().simple().to_string(),
        workspace_root: canonical.to_string_lossy().into_owned(),
        skills: to_preserve.clone(),
    };
    // 先持久化意图: 从此刻起任何中断都可凭回执续行。
    persist_receipt(workspace, &receipt).await?;
    for name in &to_preserve {
        if let Err(error) = move_dir(&skills_dir.join(name), &area.join(name)).await {
            tracing::error!(
                %error,
                preserve_area = %area.display(),
                skill = %name,
                "preserving locked skill failed; already-moved copies stay in the preserve area; re-entering workspace creation resumes from the receipt"
            );
            return Err(AppError::system(format!(
                "preserve locked skill '{name}' failed: {error}; moved copies stay at {} and will be resumed by the next create_workspace on this workspace",
                area.display()
            )));
        }
    }
    Ok((Some(PreserveAreaHandle { area }), to_preserve))
}

async fn persist_receipt(workspace: &Path, receipt: &PreserveReceipt) -> AppResult<()> {
    let receipt_path = preserve_receipt_path(workspace);
    let content = serde_json::to_vec(receipt)
        .map_err(|error| AppError::system(format!("serialize preserve receipt: {error}")))?;
    fs::write(&receipt_path, content).await.map_err(|error| {
        AppError::system(format!(
            "write preserve receipt {}: {error}",
            receipt_path.display()
        ))
    })?;
    Ok(())
}

/// 保留区句柄（本次操作视角）: 恢复确认前不自动清理, Drop 不删除。
pub(crate) struct PreserveAreaHandle {
    #[allow(dead_code)]
    area: PathBuf,
}

/// 还原保留的 skill 子目录（幂等）, 全部确认后清理回执与保留区。
async fn restore_locked_skills(
    preserved: &(Option<PreserveAreaHandle>, Vec<String>),
    workspace: &Path,
) -> AppResult<()> {
    let (holder, names) = preserved;
    let Some(_area) = holder.as_ref() else {
        return Ok(());
    };
    if names.is_empty() {
        return Ok(());
    }
    let receipt = PreserveReceipt {
        version: PRESERVE_RECEIPT_VERSION,
        operation_id: uuid::Uuid::now_v7().simple().to_string(),
        workspace_root: workspace.to_string_lossy().into_owned(),
        skills: names.clone(),
    };
    restore_from_receipt(workspace, &receipt).await?;
    confirm_preservation_finished(workspace, &receipt).await
}

async fn read_optional_text(path: &Path) -> AppResult<Option<String>> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::system(format!(
            "read {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::helpers::now_nanos;
    use super::*;

    fn seed_locked_skill(workspace: &Path, name: &str, marker: &str) {
        let dir = workspace.join(".agents").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("skill dir");
        std::fs::write(dir.join(DYNAMIC_ADD_LOCK), b"").expect("lock");
        std::fs::write(dir.join("SKILL.md"), format!("# {marker}")).expect("content");
    }

    /// P1-2 反例(中断恢复闭环): preserve 完成、restore 被取消（模拟进程中断/
    /// 请求取消）后, 原入口重试 `create_workspace` 必须凭持久回执恢复**全部**
    /// 技能。修复前的实现入口只扫描当前 skills/, 重试返回成功而唯一副本留在
    /// 不可达的随机目录（Codex remaining-data-protection.md §1 源码分析）。
    #[tokio::test]
    async fn interrupted_preservation_resumes_on_reentry() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        seed_locked_skill(&workspace, "skill-a", "a-original");
        seed_locked_skill(&workspace, "skill-b", "b-original");

        // preserve 完成（回执+两份副本就位）, 然后模拟中断: 不再 restore。
        let preserved =
            preserve_locked_skills(&workspace.join(".agents").join("skills"), &workspace)
                .await
                .expect("preserve");
        assert_eq!(preserved.1, ["skill-a", "skill-b"]);
        assert!(
            preserve_receipt_path(&workspace).is_file(),
            "receipt must be durable before any move"
        );
        assert!(
            !workspace
                .join(".agents")
                .join("skills")
                .join("skill-a")
                .exists()
        );
        drop(preserved); // guard drop 不删除（FS-05 语义保持）

        // 重试入口（等价于新进程对同一工作区调用 create_workspace 的前置步骤）
        resume_unfinished_preservation(&workspace)
            .await
            .expect("resume must complete the interrupted preservation");

        let skills = workspace.join(".agents").join("skills");
        for (name, marker) in [("skill-a", "a-original"), ("skill-b", "b-original")] {
            let restored = skills.join(name);
            assert!(
                restored.join(DYNAMIC_ADD_LOCK).is_file(),
                "{name} lock restored"
            );
            assert_eq!(
                fs::read_to_string(restored.join("SKILL.md")).await.unwrap(),
                format!("# {marker}"),
                "{name} content restored losslessly"
            );
        }
        assert!(
            !preserve_receipt_path(&workspace).exists(),
            "confirmed receipt must be cleaned"
        );
        assert!(
            !preserve_area(&workspace).exists(),
            "confirmed area must be cleaned"
        );
    }

    /// P1-2 反例(部分恢复幂等续行): skill-a 已恢复、skill-b 因目标被占失败 →
    /// 清除冲突后原入口重试: 全部恢复, 且已恢复的 skill-a 内容**不被覆盖**。
    #[tokio::test]
    async fn partial_restore_resumes_idempotently_without_overwrite() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        seed_locked_skill(&workspace, "skill-a", "a-original");
        seed_locked_skill(&workspace, "skill-b", "b-original");
        let preserved =
            preserve_locked_skills(&workspace.join(".agents").join("skills"), &workspace)
                .await
                .expect("preserve");

        // 第一次 restore: skill-a 成功; skill-b 目标被普通文件占用 → 失败。
        fs::write(
            workspace.join(".agents").join("skills").join("skill-b"),
            b"placeholder",
        )
        .await
        .expect("placeholder");
        assert!(
            restore_locked_skills(&preserved, &workspace).await.is_err(),
            "occupied target must fail the first restore"
        );
        // skill-a 已回到权威位; 失败后回执与 skill-b 副本必须保留。
        assert!(preserve_receipt_path(&workspace).is_file());
        assert!(preserve_area(&workspace).join("skill-b/SKILL.md").is_file());

        // 清除冲突 → 原入口重试: skill-b 恢复, skill-a 保持第一次恢复的内容。
        fs::remove_file(workspace.join(".agents").join("skills").join("skill-b"))
            .await
            .expect("clear placeholder");
        resume_unfinished_preservation(&workspace)
            .await
            .expect("retry must finish the remaining skill");
        let skills = workspace.join(".agents").join("skills");
        assert_eq!(
            fs::read_to_string(skills.join("skill-a/SKILL.md"))
                .await
                .unwrap(),
            "# a-original",
            "already-restored skill must not be overwritten"
        );
        assert_eq!(
            fs::read_to_string(skills.join("skill-b/SKILL.md"))
                .await
                .unwrap(),
            "# b-original"
        );
        assert!(!preserve_receipt_path(&workspace).exists());
    }

    /// P1-2 反例(preserve 中途失败精确续行): 回执含两项, 第一项已移入保留区、
    /// 第二项仍在原位（第二项 move 失败的中断态）。续行必须两项都落回权威位:
    /// 保留区副本搬回 + 原位项视为已恢复, 不重复移动、不丢失。
    #[tokio::test]
    async fn failed_second_preserve_resumes_exactly() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        seed_locked_skill(&workspace, "skill-a", "a-original");
        seed_locked_skill(&workspace, "skill-b", "b-original");

        // 手工构造"第二项 preserve 失败"的持久中断态: 回执记录两项,
        // 仅 skill-a 实际移入保留区; skill-b 留在 skills/ 原位。
        let area = preserve_area(&workspace);
        fs::create_dir_all(&area).await.expect("area");
        let skills = workspace.join(".agents").join("skills");
        fs::rename(skills.join("skill-a"), area.join("skill-a"))
            .await
            .expect("move first");
        let canonical = fs::canonicalize(&workspace).await.unwrap();
        persist_receipt(
            &workspace,
            &PreserveReceipt {
                version: PRESERVE_RECEIPT_VERSION,
                operation_id: "op".into(),
                workspace_root: canonical.to_string_lossy().into_owned(),
                skills: vec!["skill-a".into(), "skill-b".into()],
            },
        )
        .await
        .expect("receipt");

        resume_unfinished_preservation(&workspace)
            .await
            .expect("resume from exact receipt");

        assert_eq!(
            fs::read_to_string(skills.join("skill-a/SKILL.md"))
                .await
                .unwrap(),
            "# a-original",
            "moved copy must come back from the preserve area"
        );
        assert_eq!(
            fs::read_to_string(skills.join("skill-b/SKILL.md"))
                .await
                .unwrap(),
            "# b-original",
            "in-place item must stay authoritative"
        );
        assert!(!preserve_receipt_path(&workspace).exists());
    }

    /// P1-2 反例(两请求竞争): 同一工作区并发 create_workspace 不得互相删除
    /// 保留来源或重复宣称完成——进程内按工作区互斥串行化。
    #[tokio::test]
    async fn concurrent_reentry_keeps_single_copy_and_completes() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        seed_locked_skill(&workspace, "skill-a", "a-original");

        let (first, second) = tokio::join!(
            create_workspace(&workspace, None, Vec::new(), None, None),
            create_workspace(&workspace, None, Vec::new(), None, None),
        );
        first.expect("first create succeeds");
        second.expect("serialized second create succeeds");
        let skill = workspace.join(".agents").join("skills").join("skill-a");
        assert_eq!(
            fs::read_to_string(skill.join("SKILL.md")).await.unwrap(),
            "# a-original",
            "exactly one authoritative copy survives the race"
        );
        assert!(!preserve_receipt_path(&workspace).exists());
        assert!(!preserve_area(&workspace).exists());
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

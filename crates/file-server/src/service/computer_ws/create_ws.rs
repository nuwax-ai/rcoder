//! create-workspace (对齐 nuwax computerUtils.createWorkspace + AgentWorkspaceUtils):
//! `.agents/{skills,agents}` 装配 + `.dynamic_add.lock` 保留 + skill/agent zip/url 合并 + syncAgents。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::models::SkillFailure;

use super::helpers::{find_dir, move_dir};
use super::preservation::{
    self, WorkspaceGuard, before_rebuild, preserve_locked_skills, restore_locked_skills,
    resume_unfinished_preservation,
};

const MAX_WORKSPACE_FILE_WORKERS: usize = 4;

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
    let guard = preservation::acquire(workspace).await?;
    static WORKERS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| {
            std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_WORKSPACE_FILE_WORKERS))
        });
    let permit = WORKERS
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::system("workspace file worker capacity is closed"))?;
    let skill_zip = skill_zip.map(Path::to_path_buf);
    let downloader = downloader.cloned();
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|error| AppError::system(format!("workspace runtime unavailable: {error}")))?;
    let request_id = crate::error::current_request_id();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let _worker = std::thread::Builder::new()
        .name("workspace-skills".to_string())
        .spawn(move || {
            let _permit = permit;
            // The actual worker owns both locks and the input snapshot. Dropping an
            // HTTP observer cannot release locks while filesystem work continues.
            let result = (|| {
                let snapshot = skill_zip.as_deref().map(snapshot_skill_zip).transpose()?;
                guard.validate()?;
                let result = runtime.block_on(crate::error::REQUEST_ID.scope(
                    request_id,
                    create_workspace_locked(
                        &guard,
                        snapshot.as_ref().map(tempfile::NamedTempFile::path),
                        skill_urls,
                        hook_config,
                        downloader.as_ref(),
                    ),
                ));
                if result.is_ok() {
                    guard.validate()?;
                }
                result
            })();
            if sender.send(result).is_err() {
                tracing::debug!("workspace file worker completed after its observer disconnected");
            }
        })
        .map_err(|error| AppError::system(format!("start workspace file worker: {error}")))?;
    receiver.await.map_err(|error| {
        AppError::system(format!(
            "workspace file worker exited without a result: {error}"
        ))
    })?
}

fn snapshot_skill_zip(path: &Path) -> AppResult<tempfile::NamedTempFile> {
    #[cfg(unix)]
    let mut input = {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            AppError::system(format!("open skill archive {}: {error}", path.display()))
        })?;
        std::fs::File::from(fd)
    };
    #[cfg(not(unix))]
    let mut input = std::fs::File::open(path)?;
    if !input.metadata()?.is_file() {
        return Err(AppError::validation("skill archive must be a regular file"));
    }
    let mut snapshot = tempfile::Builder::new().suffix(".zip").tempfile()?;
    std::io::copy(&mut input, snapshot.as_file_mut()).map_err(|error| {
        AppError::system(format!(
            "snapshot skill archive {}: {error}",
            path.display()
        ))
    })?;
    snapshot.as_file().sync_all()?;
    Ok(snapshot)
}

async fn create_workspace_locked(
    guard: &WorkspaceGuard,
    skill_zip: Option<&Path>,
    skill_urls: Vec<String>,
    hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
    downloader: Option<&crate::service::skill_download::SkillDownloader>,
) -> AppResult<CreateWorkspaceResult> {
    let workspace = guard.root();
    let start = std::time::Instant::now();
    // P1-2: 先续行上次未完成的保留恢复（精确回执驱动, 不扫描随机目录）。
    // 重建进程/取消后的重试从此处闭环: 部分恢复幂等继续, 未确认不虚报完成。
    resume_unfinished_preservation(workspace).await?;

    let (skills_dir, agents_dir) = ensure_primary_agent_dirs(workspace).await?;

    // 保留含 .dynamic_add.lock 的 skill 子目录 (agents 无此逻辑)
    let preserved = preserve_locked_skills(&skills_dir, workspace).await?;
    #[cfg(test)]
    preservation::test_gate::after_preserve(workspace)?;

    // rm + 重建 skills/agents
    guard.validate()?;
    before_rebuild(&preserved, workspace)?;
    remove_dir_all_if_exists(&skills_dir).await?;
    remove_dir_all_if_exists(&agents_dir).await?;
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;

    // 还原保留 skills（幂等; 失败时唯一副本留在保留区, 重试入口按回执续行）
    restore_locked_skills(&preserved, workspace).await?;
    guard.validate()?;

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

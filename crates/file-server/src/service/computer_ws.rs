//! computer 工作区装配 (对齐 nuwax `computerUtils.createWorkspace` +
//! `computerFileUtils.importProject` + `AgentWorkspaceUtils`)。
//!
//! 集中处理:
//! - import-project: `.zip` 校验 + 解压 + `removeTopLevelDir` + 白名单保留合并
//! - create-workspace: `.agents/{skills,agents}` 装配 + `.dynamic_add.lock` 保留 + skill/agent zip 合并 + syncAgents

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};

/// 导入项目时保留的目录/文件 (对齐 nuwax IMPORT_PROJECT_PRESERVED_ENTRIES)。
const IMPORT_PRESERVED: &[&str] = &[
    ".git",
    ".agents",
    ".claude",
    ".codex",
    ".opencode",
    ".tmp",
    ".logs",
];

/// `.dynamic_add.lock` 标记 (对齐 nuwax DYNAMIC_ADD_LOCK; 含此锁的 skill 子目录不删)。
const DYNAMIC_ADD_LOCK: &str = ".dynamic_add.lock";

// ── import-project ─────────────────────────────────────────────────────────────

pub struct ImportResult {
    pub user_id: String,
    pub cid: String,
    pub target_dir: String,
}

/// import-project 核心 (对齐 nuwax importProject):
/// 1. 写临时 zip + 解压到 extractRoot (失败则不动 target)
/// 2. `remove_top_level_dir` (单顶层目录上提一层)
/// 3. 备份 target 非白名单条目 → backupDir
/// 4. 清空 target 非白名单条目
/// 5. 合并 extractRoot → target (跳过白名单名), 失败则从 backupDir 回滚
/// 6. 清理 backupDir + extractRoot
pub async fn import_project(target_dir: &Path, zip_data: Vec<u8>) -> AppResult<ImportResult> {
    // 临时解压目录 (target 父目录下, 便于同设备 rename)
    let extract_root = temp_sibling(target_dir, "import_extract");
    fs::create_dir_all(&extract_root).await?;
    let tmp_zip = extract_root.join("src.zip");
    // 解压 (zip::extract_to 内部 safe_zip_entry 防穿越)
    let extract_res: AppResult<()> = async {
        fs::write(&tmp_zip, &zip_data).await?;
        crate::service::zip::extract_to(tmp_zip.clone(), extract_root.clone()).await
    }
    .await;
    if let Err(e) = extract_res {
        let _ = fs::remove_dir_all(&extract_root).await;
        return Err(e);
    }
    // 单顶层目录上提
    remove_top_level_dir(&extract_root, &[]).await;

    // 备份 target 非白名单条目 (移动到 backupDir)
    let backup_dir = temp_sibling(target_dir, "import_backup");
    let _ = fs::remove_dir_all(&backup_dir).await;
    backup_except_preserved(target_dir, &backup_dir).await?;

    // 清空 target 非白名单条目 (此时 target 仅剩白名单)
    clear_except_preserved(target_dir).await?;

    // 合并 extractRoot → target (跳过白名单名), 失败回滚
    let merge_res = merge_extracted(&extract_root, target_dir).await;
    if let Err(merge_err) = merge_res {
        // 回滚: 清空刚合并的非白名单条目 + 从备份恢复
        let _ = clear_except_preserved(target_dir).await;
        let _ = restore_from_backup(&backup_dir, target_dir).await;
        let _ = fs::remove_dir_all(&extract_root).await;
        let _ = fs::remove_dir_all(&backup_dir).await;
        return Err(merge_err);
    }
    // 成功: 清理
    let _ = fs::remove_dir_all(&extract_root).await;
    let _ = fs::remove_dir_all(&backup_dir).await;

    Ok(ImportResult {
        user_id: String::new(),
        cid: String::new(),
        target_dir: target_dir.to_string_lossy().to_string(),
    })
}

/// 把 target 非白名单条目移动到 backup_dir (对齐 nuwax backupWorkspaceExceptPreserved)。
async fn backup_except_preserved(target: &Path, backup_dir: &Path) -> AppResult<()> {
    fs::create_dir_all(backup_dir).await?;
    let mut entries = fs::read_dir(target).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = backup_dir.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

/// 清空 target 非白名单条目 (对齐 nuwax clearWorkspaceExceptPreserved)。
async fn clear_except_preserved(target: &Path) -> AppResult<()> {
    let mut entries = fs::read_dir(target).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type().await?;
        let r = if ft.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        // 不存在视为已清 (NotFound 不计错)
        if let Err(e) = r
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(AppError::system(format!("clear {}: {e}", path.display())));
        }
    }
    Ok(())
}

/// 合并 extract_root 内容到 target (跳过白名单名, 对齐 nuwax mergeExtractedIntoWorkspace)。
async fn merge_extracted(extract_root: &Path, target: &Path) -> AppResult<()> {
    fs::create_dir_all(target).await?;
    let mut entries = fs::read_dir(extract_root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 跳过白名单名 (保留 target 已有的 .git/.agents 等)
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

/// 从 backup_dir 恢复非白名单条目到 target (对齐 nuwax restoreWorkspaceFromBackup)。
async fn restore_from_backup(backup_dir: &Path, target: &Path) -> AppResult<()> {
    if !fs::try_exists(backup_dir).await.unwrap_or(false) {
        return Ok(());
    }
    fs::create_dir_all(target).await?;
    let mut entries = fs::read_dir(backup_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if IMPORT_PRESERVED.contains(&name.as_str()) {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        move_dir(&src, &dst).await?;
    }
    Ok(())
}

// ── create-workspace ───────────────────────────────────────────────────────────

pub struct CreateWorkspaceResult {
    pub message: String,
    pub workspace_root: String,
    pub updated_skills: Vec<String>,
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
    skill_zip: Option<Vec<u8>>,
    skill_urls: Vec<String>,
    hook_config: Option<crate::service::agent_hooks::HookConfigInput>,
) -> AppResult<CreateWorkspaceResult> {
    let (skills_dir, agents_dir) = ensure_primary_agent_dirs(workspace).await?;

    // 保留含 .dynamic_add.lock 的 skill 子目录 (agents 无此逻辑)
    let preserved = preserve_locked_skills(&skills_dir, workspace).await?;

    // rm + 重建 skills/agents
    let _ = fs::remove_dir_all(&skills_dir).await;
    let _ = fs::remove_dir_all(&agents_dir).await;
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;

    // 还原保留 skills
    restore_locked_skills(&preserved, &skills_dir).await?;

    // 写入 agent hook 配置 (best-effort: 内部吞错不破坏 workspace 创建, 对齐 nuwax try/catch)
    let _ = crate::service::agent_hooks::write_agent_hook_configs(
        workspace,
        hook_config.unwrap_or_default(),
    )
    .await;

    let mut updated_skills: Vec<String> = Vec::new();
    let mut updated_dirs: Vec<&str> = Vec::new();
    let had_file = skill_zip.is_some();
    let has_urls = !skill_urls.is_empty();

    // 无 file 且无 skillUrls → syncAgents + 早退 (对齐 nuwax)
    if !had_file && !has_urls {
        crate::service::skills::sync_agents(workspace).await?;
        return Ok(CreateWorkspaceResult {
            message: "Workspace created (no uploaded file, no skills and agents)".to_string(),
            workspace_root: workspace.to_string_lossy().to_string(),
            updated_skills,
        });
    }

    if let Some(data) = skill_zip {
        // 解压到临时目录 (zip 内找 skills/ + agents/)
        let extract_root = temp_sibling(workspace, "skill_extract");
        fs::create_dir_all(&extract_root).await?;
        let tmp_zip = extract_root.join("src.zip");
        let extract_res: AppResult<()> = async {
            fs::write(&tmp_zip, &data).await?;
            crate::service::zip::extract_to(tmp_zip.clone(), extract_root.clone()).await
        }
        .await;
        match extract_res {
            Ok(()) => {
                // skills/: 逐子目录移动覆盖
                if let Some(src_skills) = find_dir(&extract_root, "skills") {
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
                if let Some(src_agents) = find_dir(&extract_root, "agents") {
                    let _ = fs::remove_dir_all(&agents_dir).await;
                    move_dir(&src_agents, &agents_dir).await?;
                    updated_dirs.push("agents");
                }
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&extract_root).await;
                return Err(e);
            }
        }
        let _ = fs::remove_dir_all(&extract_root).await;
    }

    // skillUrls: 逐个下载解压, 集成 skill 目录 (对齐 nuwax createWorkspace skillUrls 循环)
    for url in &skill_urls {
        match process_skill_url(url, &skills_dir, workspace).await {
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
            }
        }
    }

    // syncAgents: .agents → .claude/.opencode/.codex
    crate::service::skills::sync_agents(workspace).await?;

    let message = if updated_dirs.is_empty() {
        "Workspace created successfully (skills and agents directories not found)".to_string()
    } else {
        format!(
            "Workspace created successfully, {} updated",
            updated_dirs.join(" and ")
        )
    };

    Ok(CreateWorkspaceResult {
        message,
        workspace_root: workspace.to_string_lossy().to_string(),
        updated_skills,
    })
}

/// 处理单个 skillUrl: 下载 zip → 解压 → 集成 skill 目录到 skills_dir
/// (对齐 nuwax createWorkspace skillUrls: 若含 skills/ 目录则取其子目录, 否则取顶层非隐藏目录)。
async fn process_skill_url(
    url: &str,
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<Vec<String>> {
    let data = crate::service::skills::fetch_url(url).await?;
    let extract_root = temp_sibling(workspace, "skill_url_extract");
    fs::create_dir_all(&extract_root).await?;
    let tmp_zip = extract_root.join("src.zip");
    let extract_res: AppResult<()> = async {
        fs::write(&tmp_zip, &data).await?;
        crate::service::zip::extract_to(tmp_zip.clone(), extract_root.clone()).await
    }
    .await;
    if let Err(e) = extract_res {
        let _ = fs::remove_dir_all(&extract_root).await;
        return Err(e);
    }
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
    let _ = fs::remove_dir_all(&extract_root).await;
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

/// 把含 `.dynamic_add.lock` 的 skill 子目录移到临时区保留 (对齐 nuwax hasDynamicAddLock)。
/// 返回 (临时目录, 保留的 skill 名列表)。
async fn preserve_locked_skills(
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<(PathBuf, Vec<String>)> {
    let preserved: Vec<String> = Vec::new();
    if !fs::try_exists(skills_dir).await.unwrap_or(false) {
        return Ok((PathBuf::new(), preserved));
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
        if fs::try_exists(&lock).await.unwrap_or(false) {
            to_preserve.push(name);
        }
    }
    if to_preserve.is_empty() {
        return Ok((PathBuf::new(), Vec::new()));
    }
    let temp = temp_sibling(workspace, "preserved_skills");
    fs::create_dir_all(&temp).await?;
    for name in &to_preserve {
        move_dir(&skills_dir.join(name), &temp.join(name)).await?;
    }
    Ok((temp, to_preserve))
}

/// 还原保留的 skill 子目录。
async fn restore_locked_skills(
    preserved: &(PathBuf, Vec<String>),
    skills_dir: &Path,
) -> AppResult<()> {
    let (temp, names) = preserved;
    if temp.as_os_str().is_empty() || names.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(skills_dir).await?;
    for name in names {
        move_dir(&temp.join(name), &skills_dir.join(name)).await?;
    }
    let _ = fs::remove_dir_all(temp).await;
    Ok(())
}

// ── 共享 helpers ────────────────────────────────────────────────────────────────

/// 单顶层目录上提一层 (对齐 nuwax removeTopLevelDir / removeTopLevelFolder):
/// 过滤隐藏项 (`.` 开头) + `node_modules` + extra 噪声名, 若仅剩 1 个目录, 则内容上提。
pub async fn remove_top_level_dir(dir: &Path, extra_excludes: &[&str]) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    let mut filtered: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if name == "node_modules" || extra_excludes.contains(&name.as_str()) {
            continue;
        }
        if let Ok(ft) = entry.file_type().await
            && ft.is_dir()
        {
            filtered.push(entry.path());
        }
    }
    if filtered.len() != 1 {
        return;
    }
    let Some(only) = filtered.into_iter().next() else {
        return;
    };
    // 唯一顶层目录内容上提: rename 到临时名, 再逐项 rename 回 dir
    let staging = dir.join(format!(".toplift_{}", now_nanos()));
    if fs::rename(&only, &staging).await.is_err() {
        return;
    }
    if let Ok(mut rd) = fs::read_dir(&staging).await {
        while let Ok(Some(child)) = rd.next_entry().await {
            let name = child.file_name();
            let _ = move_dir(&child.path(), &dir.join(&name)).await;
        }
    }
    let _ = fs::remove_dir_all(&staging).await;
}

/// 移动目录 (rename; 跨设备 fallback copy + rm, 对齐 nuwax moveDirectory EXDEV 降级)。
async fn move_dir(src: &Path, dst: &Path) -> AppResult<()> {
    if fs::rename(src, dst).await.is_err() {
        // rename 失败 (跨设备 EXDEV / 其他) → copy + rm; copy 层会抛真实错误
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
        let _ = fs::remove_dir_all(src).await;
    }
    Ok(())
}

/// 在 base 的父(或自身)目录下建临时目录名 (尽量同设备, 便于 rename)。
fn temp_sibling(base: &Path, prefix: &str) -> PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new("/tmp"));
    parent.join(format!(".{prefix}_{}", now_nanos()))
}

/// 在 root 下查找 `name` 目录: 优先 root/name, 再查一层子目录 name/ (对齐 nuwax findDir)。
fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.is_dir() {
        return Some(direct);
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let sub = entry.path().join(name);
            if sub.is_dir() {
                return Some(sub);
            }
        }
    }
    None
}

/// 当前时间纳秒 (仅用于生成唯一临时名; 避免直接 `new Date`)。
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_entries_match_nuwax() {
        // 7 项白名单 (对齐 nuwax IMPORT_PROJECT_PRESERVED_ENTRIES)
        assert_eq!(IMPORT_PRESERVED.len(), 7);
        for e in [
            ".git",
            ".agents",
            ".claude",
            ".codex",
            ".opencode",
            ".tmp",
            ".logs",
        ] {
            assert!(IMPORT_PRESERVED.contains(&e), "missing {e}");
        }
    }

    #[tokio::test]
    async fn remove_top_level_dir_lifts_single_dir() {
        let tmp = std::env::temp_dir().join(format!("fs_rtl_{}", now_nanos()));
        fs::create_dir_all(tmp.join("only").join("deep"))
            .await
            .unwrap();
        fs::write(tmp.join("only").join("a.txt"), "x")
            .await
            .unwrap();
        remove_top_level_dir(&tmp, &[]).await;
        assert!(tmp.join("a.txt").exists());
        assert!(tmp.join("deep").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn remove_top_level_dir_noop_on_multi() {
        let tmp = std::env::temp_dir().join(format!("fs_rtm_{}", now_nanos()));
        fs::create_dir_all(tmp.join("a")).await.unwrap();
        fs::create_dir_all(tmp.join("b")).await.unwrap();
        remove_top_level_dir(&tmp, &[]).await;
        assert!(tmp.join("a").is_dir());
        assert!(tmp.join("b").is_dir());
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn import_project_preserves_dotgit() {
        let tmp = std::env::temp_dir().join(format!("fs_imp_{}", now_nanos()));
        // 现有工作区含 .git (需保留) 和 old.txt (应被覆盖)
        fs::create_dir_all(tmp.join(".git").join("refs"))
            .await
            .unwrap();
        fs::write(tmp.join(".git").join("HEAD"), "ref")
            .await
            .unwrap();
        fs::write(tmp.join("old.txt"), "old").await.unwrap();
        // 打包新 zip: src/ 下含 new.txt (单顶层 src → 上提)
        let zip_root = std::env::temp_dir().join(format!("fs_zip_{}", now_nanos()));
        fs::create_dir_all(zip_root.join("src")).await.unwrap();
        fs::write(zip_root.join("src").join("new.txt"), "new")
            .await
            .unwrap();
        let zip_path = zip_root.join("out.zip");
        crate::service::zip::pack_dir(zip_root.clone(), zip_path.clone(), Vec::new(), Vec::new())
            .await
            .unwrap();
        let zip_data = fs::read(&zip_path).await.unwrap();
        let _ = import_project(&tmp, zip_data).await.unwrap();
        // .git 保留, old.txt 被移除, new.txt 出现
        assert!(tmp.join(".git").join("HEAD").exists());
        assert!(!tmp.join("old.txt").exists());
        assert!(tmp.join("new.txt").exists());
        let _ = fs::remove_dir_all(&tmp).await;
        let _ = fs::remove_dir_all(&zip_root).await;
    }

    #[tokio::test]
    async fn create_workspace_writes_agents_skills() {
        let tmp = std::env::temp_dir().join(format!("fs_cw_{}", now_nanos()));
        let res = create_workspace(&tmp, None, Vec::new(), None)
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
        let _ = fs::remove_dir_all(&tmp).await;
    }
}

//! Agent-store: 智能体级实体存储 (对齐 TS `agentStoreUtils.js`, commit 659047c + 3c8ffc3)。
//!
//! 目录结构: `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/{agentId}/{skills,agents}/`
//! 与会话工作区 `{COMPUTER_WORKSPACE_DIR}/{userId}/{cId}` 同属一棵树。
//!
//! 核心能力:
//! - 锁机制 (`.sync.lock` 独占写, 5min 陈旧抢占)
//! - 技能安装/覆盖 (`install_skill_dir`)
//! - agents 整体替换 (`replace_agents_dir`)
//! - 差集清理 (`prune_agent_skills`, 保留 `.dynamic_add.lock` 的)
//! - 按需安装判断 (`agent_skill_exists`)

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppResult;

const DYNAMIC_ADD_LOCK: &str = ".dynamic_add.lock";
const SYNC_LOCK_NAME: &str = ".sync.lock";
/// 锁过期时间: 5 分钟 (避免异常退出后永久卡死, 对齐 TS `SYNC_LOCK_STALE_MS`)。
const SYNC_LOCK_STALE_MS: u64 = 5 * 60 * 1000;

/// 智能体级实体存储路径: `{workspace_root}/{user_id}/.agent-store/{agent_id}`。
pub fn agent_store_path(workspace_root: &Path, user_id: &str, agent_id: &str) -> PathBuf {
    workspace_root
        .join(user_id)
        .join(".agent-store")
        .join(agent_id)
}

/// 创建实体目录 `{agent_store}/{skills,agents}`, 返回两个子目录路径。
pub async fn ensure_agent_store_dirs(
    workspace_root: &Path,
    user_id: &str,
    agent_id: &str,
) -> AppResult<(PathBuf, PathBuf)> {
    let store = agent_store_path(workspace_root, user_id, agent_id);
    let skills_dir = store.join("skills");
    let agents_dir = store.join("agents");
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;
    Ok((skills_dir, agents_dir))
}

/// 文件锁 guard: acquire 返回 `Some` 表示拿到锁, `None` 表示被占用 (调用方跳过更新)。
pub struct AgentStoreLock {
    lock_path: PathBuf,
}

impl AgentStoreLock {
    /// 尝试获取写锁 (`O_CREAT | O_EXCL` 独占创建)。
    /// 拿不到则检查陈旧度, 超过 `SYNC_LOCK_STALE_MS` 可抢占。
    /// 返回 `None` 表示锁被占用且未过期 → 调用方跳过实体更新。
    pub async fn try_acquire(
        workspace_root: &Path,
        user_id: &str,
        agent_id: &str,
    ) -> AppResult<Option<Self>> {
        let store = agent_store_path(workspace_root, user_id, agent_id);
        fs::create_dir_all(&store).await?;
        let lock_path = store.join(SYNC_LOCK_NAME);

        // 尝试独占创建
        match fs::File::create(&lock_path).await {
            Ok(_file) => {
                // 写入 pid:timestamp (对齐 TS)
                let content = format!(
                    "{}:{}",
                    std::process::id(),
                    chrono::Utc::now().timestamp_millis()
                );
                if let Err(e) = fs::write(&lock_path, content).await {
                    tracing::warn!(error = %e, "write lock content failed");
                }
                Ok(Some(Self { lock_path }))
            }
            Err(_) => {
                // 锁已存在, 检查陈旧度
                let meta = match fs::metadata(&lock_path).await {
                    Ok(m) => m,
                    Err(_) => return Ok(None), // 刚好被释放了, 但本次跳过
                };
                let modified = meta.modified();
                let is_stale = modified.ok().is_none_or(|t| {
                    t.elapsed()
                        .map_or(true, |e| e.as_millis() as u64 > SYNC_LOCK_STALE_MS)
                });
                if is_stale {
                    tracing::warn!(lock = %lock_path.display(), "stealing stale agent store lock");
                    if let Err(e) = fs::remove_file(&lock_path).await {
                        tracing::warn!(error = %e, "remove stale lock failed");
                    }
                    match fs::File::create(&lock_path).await {
                        Ok(_f) => Ok(Some(Self { lock_path })),
                        Err(_) => Ok(None),
                    }
                } else {
                    tracing::info!(lock = %lock_path.display(), "agent store lock held, skipping update");
                    Ok(None)
                }
            }
        }
    }
}

impl Drop for AgentStoreLock {
    fn drop(&mut self) {
        let path = self.lock_path.clone();
        // best-effort 删除锁文件 (对齐 TS releaseAgentStoreLock)
        tokio::spawn(async move {
            if let Err(e) = fs::remove_file(&path).await {
                tracing::warn!(error = %e, "release agent store lock failed");
            }
        });
    }
}

/// 检查 skill 目录是否有 `.dynamic_add.lock`。
fn has_dynamic_add_lock(skill_path: &Path) -> bool {
    skill_path.join(DYNAMIC_ADD_LOCK).exists()
}

/// 写入 `.dynamic_add.lock` (标记动态添加的技能)。
async fn ensure_dynamic_add_lock(skill_path: &Path) -> AppResult<()> {
    fs::create_dir_all(skill_path).await?;
    fs::write(
        skill_path.join(DYNAMIC_ADD_LOCK),
        format!("{}\n", chrono::Utc::now().timestamp_millis()),
    )
    .await?;
    Ok(())
}

/// 按 `keep_names` 清理实体 skills:
/// - 不在 keep 列表且无 `.dynamic_add.lock` → 删除
/// - 不在 keep 列表但有 `.dynamic_add.lock` → 保留
/// - 在 keep 列表 → 保留
pub async fn prune_agent_skills(
    skills_dir: &Path,
    keep_names: &[String],
) -> AppResult<(Vec<String>, Vec<String>)> {
    let keep: std::collections::HashSet<&str> = keep_names
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if !skills_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut removed = Vec::new();
    let mut kept_dynamic = Vec::new();
    let mut entries = fs::read_dir(skills_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if keep.contains(name.as_str()) {
            continue;
        }
        let skill_path = entry.path();
        if has_dynamic_add_lock(&skill_path) {
            kept_dynamic.push(name);
        } else {
            fs::remove_dir_all(&skill_path).await?;
            removed.push(name);
        }
    }

    tracing::info!(
        skills_dir = %skills_dir.display(),
        keep_count = keep.len(),
        removed = ?removed,
        kept_dynamic = ?kept_dynamic,
        "prune agent skills completed"
    );
    Ok((removed, kept_dynamic))
}

/// 将源 skill 目录覆盖写入目标 (同名覆盖)。
/// `as_dynamic=true` → 写入后打 `.dynamic_add.lock`; `false` → 清除已有锁。
pub async fn install_skill_dir(
    src: &Path,
    dest_skills_dir: &Path,
    skill_name: &str,
    as_dynamic: bool,
) -> AppResult<()> {
    let dest = dest_skills_dir.join(skill_name);
    // 删旧目标 (同名覆盖)
    match fs::remove_dir_all(&dest).await {
        Ok(()) | Err(_) => {}
    }
    fs::create_dir_all(dest_skills_dir).await?;
    move_or_copy_directory(src, &dest).await?;

    if as_dynamic {
        ensure_dynamic_add_lock(&dest).await?;
    } else {
        let lock = dest.join(DYNAMIC_ADD_LOCK);
        if let Err(e) = fs::remove_file(&lock).await {
            tracing::debug!(error = %e, "remove dynamic_add_lock (non-existent is ok)");
        }
    }
    Ok(())
}

/// 用源 agents 目录整体替换实体 agents (每次 createWorkspace 刷新)。
pub async fn replace_agents_dir(
    src_agents_dir: Option<&Path>,
    dest_agents_dir: &Path,
) -> AppResult<()> {
    // 删旧
    match fs::remove_dir_all(dest_agents_dir).await {
        Ok(()) | Err(_) => {}
    }
    if let Some(parent) = dest_agents_dir.parent() {
        fs::create_dir_all(parent).await?;
    }
    if let Some(src) = src_agents_dir
        && src.exists()
    {
        move_or_copy_directory(src, dest_agents_dir).await?;
        return Ok(());
    }
    // 无源 → 建空目录
    fs::create_dir_all(dest_agents_dir).await?;
    Ok(())
}

/// 判断实体 skills 下是否已有指定技能目录。
pub fn agent_skill_exists(skills_dir: &Path, skill_name: &str) -> bool {
    skills_dir.join(skill_name).is_dir()
}

/// rename 优先 (同分区秒移), EXDEV (跨设备) 回退 copy+rm。
async fn move_or_copy_directory(src: &Path, dst: &Path) -> AppResult<()> {
    match fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            // EXDEV: 跨设备, 降级为 copy + rm
            crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
            fs::remove_dir_all(src).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

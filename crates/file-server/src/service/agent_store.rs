//! Agent-store: 智能体级实体存储 (对齐 TS `agentStoreUtils.js`, commit 659047c + 3c8ffc3)。
//!
//! 目录结构: `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/{agentId}/{skills,agents}/`
//! 与会话工作区 `{COMPUTER_WORKSPACE_DIR}/{userId}/{cId}` 同属一棵树。
//!
//! 核心能力:
//! - 技能安装/覆盖 (`install_skill_dir`) — 逐个子目录原子覆盖, 天然并发安全
//! - agents 更新 (`update_agents_dir`) — 逐个子目录并发覆盖, 无锁安全
//! - 差集清理 (`prune_agent_skills`, 保留 `.dynamic_add.lock` 的)
//! - 按需安装判断 (`agent_skill_exists`)
//!
//! **无锁设计**: 所有写操作都是"逐个子目录: 删旧 → rename 移入"的原子操作。
//! 不同子目录天然无冲突; 同名子目录并发覆盖最终一致。不需要文件锁。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use futures_util::future::try_join_all;
use tokio::fs;

use crate::error::AppResult;

const DYNAMIC_ADD_LOCK: &str = ".dynamic_add.lock";

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
    // 删旧目标 (同名覆盖, NotFound 安全)
    match fs::remove_dir_all(&dest).await {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
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

/// 逐个子目录并发覆盖更新 agents 目录 (无锁安全)。
///
/// 每个子目录独立操作: 删旧 → rename 移入。不同子目录天然无冲突;
/// 同名子目录并发覆盖最终一致。目标目录始终存在, 无"目录空"窗口。
pub async fn update_agents_dir(src: Option<&Path>, dest: &Path) -> AppResult<()> {
    fs::create_dir_all(dest).await?;
    if let Some(src) = src
        && src.exists()
    {
        let mut entries = fs::read_dir(src).await?;
        let mut tasks = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let dst = dest.join(entry.file_name());
            let src_entry = entry.path();
            tasks.push(async move {
                match fs::remove_dir_all(&dst).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                move_or_copy_directory(&src_entry, &dst).await
            });
        }
        try_join_all(tasks).await?;
    }
    Ok(())
}

/// 判断实体 skills 下是否已有指定技能目录。
pub fn agent_skill_exists(skills_dir: &Path, skill_name: &str) -> bool {
    skills_dir.join(skill_name).is_dir()
}

/// rename 优先 (同分区秒移), CrossesDevices (跨设备) 回退 copy+rm。
/// 用 `ErrorKind::CrossesDevices` 跨平台检测 (Rust 1.85+ stabilized)。
async fn move_or_copy_directory(src: &Path, dst: &Path) -> AppResult<()> {
    match fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            // 跨设备: 降级为 copy + rm
            crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
            fs::remove_dir_all(src).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn install_skill_dir_copies_and_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dest_dir = tmp.path().join("skills");

        // v1
        let src1 = tmp.path().join("src1");
        fs::create_dir_all(&src1).await.unwrap();
        fs::write(src1.join("SKILL.md"), "v1").await.unwrap();
        install_skill_dir(&src1, &dest_dir, "my-skill", false)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dest_dir.join("my-skill").join("SKILL.md"))
                .await
                .unwrap(),
            "v1"
        );

        // 覆盖安装 v2 (源已被 rename 移走, 用新源)
        let src2 = tmp.path().join("src2");
        fs::create_dir_all(&src2).await.unwrap();
        fs::write(src2.join("SKILL.md"), "v2").await.unwrap();
        install_skill_dir(&src2, &dest_dir, "my-skill", false)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dest_dir.join("my-skill").join("SKILL.md"))
                .await
                .unwrap(),
            "v2"
        );
    }

    #[tokio::test]
    async fn update_agents_dir_copies_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src_agents");
        let dest = tmp.path().join("agents");
        fs::create_dir_all(src.join("reviewer")).await.unwrap();
        fs::create_dir_all(src.join("coder")).await.unwrap();
        fs::write(src.join("reviewer/agent.md"), "r").await.unwrap();
        fs::write(src.join("coder/agent.md"), "c").await.unwrap();

        update_agents_dir(Some(&src), &dest).await.unwrap();

        assert!(dest.join("reviewer/agent.md").exists());
        assert!(dest.join("coder/agent.md").exists());
    }

    #[tokio::test]
    async fn update_agents_dir_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("agents");
        // 旧内容
        fs::create_dir_all(dest.join("old-agent")).await.unwrap();
        fs::write(dest.join("old-agent/agent.md"), "old")
            .await
            .unwrap();

        // 新源
        let src = tmp.path().join("src_agents");
        fs::create_dir_all(src.join("new-agent")).await.unwrap();
        fs::write(src.join("new-agent/agent.md"), "new")
            .await
            .unwrap();

        update_agents_dir(Some(&src), &dest).await.unwrap();

        // 新的已写入
        assert_eq!(
            fs::read_to_string(dest.join("new-agent/agent.md"))
                .await
                .unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn update_agents_dir_no_source_creates_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("agents");
        update_agents_dir(None, &dest).await.unwrap();
        assert!(dest.is_dir());
    }

    #[tokio::test]
    async fn prune_removes_unlisted_non_dynamic() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        fs::create_dir_all(skills.join("keep-me")).await.unwrap();
        fs::create_dir_all(skills.join("delete-me")).await.unwrap();
        fs::create_dir_all(skills.join("dynamic-me")).await.unwrap();
        fs::write(skills.join("dynamic-me").join(DYNAMIC_ADD_LOCK), "123")
            .await
            .unwrap();

        let (removed, kept_dynamic) = prune_agent_skills(&skills, &["keep-me".to_string()])
            .await
            .unwrap();

        assert!(removed.contains(&"delete-me".to_string()));
        assert!(!removed.contains(&"dynamic-me".to_string()));
        assert!(kept_dynamic.contains(&"dynamic-me".to_string()));
        assert!(skills.join("keep-me").exists());
        assert!(!skills.join("delete-me").exists());
        assert!(skills.join("dynamic-me").exists());
    }

    #[test]
    fn agent_skill_exists_checks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("my-skill")).unwrap();
        assert!(agent_skill_exists(tmp.path(), "my-skill"));
        assert!(!agent_skill_exists(tmp.path(), "nope"));
    }
}

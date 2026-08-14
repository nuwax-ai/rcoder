//! Agent-store: 智能体级实体存储 (对齐 TS `agentStoreUtils.js` + `AgentWorkspaceUtils.js`)。
//!
//! 目录结构: `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/{agentId}/{skills,agents}/`
//! 与会话工作区 `{COMPUTER_WORKSPACE_DIR}/{userId}/{cId}` 同属一棵树。
//!
//! 核心能力:
//! - 跨平台目录链接 (`force_dir_symlink`) — Unix 相对软链 / Windows junction
//! - 工作区软链 (`link_workspace_to_agent_store`) — 软链优先, 失败 fallback copy
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

/// 智能体级实体存储路径: `{user_root}/.agent-store/{agent_id}`。
///
/// `user_root` = 会话工作区的父目录 (该 user 的稳定根), 两种 resolver 模式下均成立:
/// - Local: `{COMPUTER_WORKSPACE_DIR}/{userId}` (对齐 TS `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/...`)
/// - Subvolume (per-user PVC): `{cephfs-root}/{subvolumePath}` (user_id 已被 PVC 吸收)
///
/// store 与会话工作区 (`{user_root}/{cId}`) 同属一棵树, 相对软链可跨节点解析。
/// ⚠️ 不要从工作区叶子路径倒推多级父目录 — subvolumePath 深度不定。
pub fn agent_store_path(user_root: &Path, agent_id: &str) -> PathBuf {
    user_root.join(".agent-store").join(agent_id)
}

/// 创建实体目录 `{agent_store}/{skills,agents}`, 返回两个子目录路径。
pub async fn ensure_agent_store_dirs(
    user_root: &Path,
    agent_id: &str,
) -> AppResult<(PathBuf, PathBuf)> {
    let store = agent_store_path(user_root, agent_id);
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
            // 对齐 TS fs.rm(force): 文件和目录都能删 (skills/ 下正常是目录,
            // 但杂散文件不应让整个 prune 失败)
            if entry.file_type().await?.is_dir() {
                fs::remove_dir_all(&skill_path).await?;
            } else {
                fs::remove_file(&skill_path).await?;
            }
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

/// 检查路径是否是目录链接 (Unix symlink / Windows junction)。
/// 对齐 TS `isWorkspaceSkillsSymlinked`: push 仅在「有 agentId 且已是链接」时走 store。
/// 注意: `Path::is_symlink()` 在 Windows 上对 junction 返回 false (reparse tag 不同),
/// 需额外用 `junction::exists` 检测。
pub fn is_dir_link(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.is_symlink()
    }
    #[cfg(windows)]
    {
        path.is_symlink() || junction::exists(path).unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 跨平台目录链接 (对齐 TS `forceDirSymlink`)。
/// `link` 建为指向 `target` 的目录链接。**调用方负责删除已存在的 link。**
/// - Unix: 相对软链 (`pathdiff::diff_paths`), CephFS 跨节点绝对挂载点不同时仍可解析
/// - Windows: junction (绝对路径, 无需 SeCreateSymbolicLinkPrivilege)
pub async fn force_dir_symlink(link: &Path, target: &Path) -> AppResult<()> {
    // 确保 link 的父目录和 target 都存在
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::create_dir_all(target).await?;

    #[cfg(unix)]
    {
        let parent = link
            .parent()
            .ok_or_else(|| crate::error::AppError::system("symlink link path has no parent"))?;
        let relative = pathdiff::diff_paths(target, parent).ok_or_else(|| {
            crate::error::AppError::system("cannot compute relative symlink path")
        })?;
        fs::symlink(&relative, link).await?;
        tracing::debug!(
            link = %link.display(),
            target = %target.display(),
            relative = %relative.display(),
            "created dir symlink"
        );
    }

    #[cfg(windows)]
    {
        let abs_target = std::fs::canonicalize(target)?;
        junction::create(&abs_target, link)?;
        tracing::debug!(
            link = %link.display(),
            target = %abs_target.display(),
            "created dir junction"
        );
    }

    #[cfg(not(any(unix, windows)))]
    {
        return Err(crate::error::AppError::system(
            "symlink not supported on this platform",
        ));
    }

    Ok(())
}

/// 删除路径 (如果存在), NotFound 安全。
async fn remove_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 将会话工作区的所有 agent 目录 {skills,agents} 软链到 agent-store 实体目录
/// (对齐 TS `linkWorkspaceToAgentStore`)。
/// 优先软链所有目录; 任一失败则 fallback 为 copy 模式。
pub async fn link_workspace_to_agent_store(
    workspace: &Path,
    agent_skills_dir: &Path,
    agent_agents_dir: &Path,
) -> AppResult<()> {
    let start = std::time::Instant::now();

    // 尝试对所有 agent 目录创建软链
    let mut link_errors = Vec::new();
    for agent_dir in crate::service::skills::ALL_AGENT_DIRS {
        for (sub, store_dir) in [("skills", agent_skills_dir), ("agents", agent_agents_dir)] {
            let link = workspace.join(agent_dir).join(sub);
            remove_if_exists(&link).await?;
            if let Err(e) = force_dir_symlink(&link, store_dir).await {
                tracing::warn!(
                    agent_dir = *agent_dir,
                    sub,
                    error = %e,
                    "symlink failed, will fallback to copy"
                );
                link_errors.push(e);
            }
        }
    }

    if link_errors.is_empty() {
        tracing::info!(
            op = "link_workspace_to_agent_store",
            elapsed_ms = start.elapsed().as_millis(),
            mode = "symlink",
            "workspace linked to agent store"
        );
        return Ok(());
    }

    // Fallback: 软链失败, 从 agent-store 拷贝到 .agents, 再 sync_agents
    tracing::warn!(
        op = "link_workspace_to_agent_store",
        failed_count = link_errors.len(),
        "symlink failed, falling back to copy mode"
    );
    materialize_agent_store_by_copy(workspace, agent_skills_dir, agent_agents_dir).await?;

    tracing::info!(
        op = "link_workspace_to_agent_store",
        elapsed_ms = start.elapsed().as_millis(),
        mode = "copy-fallback",
        "workspace materialized from agent store (copy fallback)"
    );
    Ok(())
}

/// 软链失败时的 fallback: 从 agent-store 拷贝到 .agents, 再 sync_agents fan-out
/// (对齐 TS `materializeAgentStoreByCopy`)。
async fn materialize_agent_store_by_copy(
    workspace: &Path,
    agent_skills_dir: &Path,
    agent_agents_dir: &Path,
) -> AppResult<()> {
    // 清掉所有 agent 目录可能半成功的链接/目录
    for agent_dir in crate::service::skills::ALL_AGENT_DIRS {
        let root = workspace.join(agent_dir);
        remove_if_exists(&root.join("skills")).await?;
        remove_if_exists(&root.join("agents")).await?;
    }

    // 从 agent-store 拷贝到 .agents (权威源)
    let primary_skills = workspace.join(".agents").join("skills");
    let primary_agents = workspace.join(".agents").join("agents");
    fs::create_dir_all(&primary_skills).await?;
    fs::create_dir_all(&primary_agents).await?;

    if agent_skills_dir.exists() {
        crate::service::fs_util::copy_dir_filtered(agent_skills_dir, &primary_skills, &[], &[])
            .await?;
    }
    if agent_agents_dir.exists() {
        crate::service::fs_util::copy_dir_filtered(agent_agents_dir, &primary_agents, &[], &[])
            .await?;
    }

    // sync_agents fan-out (.agents → 各家 ACP 目录, 内部也是软链优先)
    crate::service::skills::sync_agents(workspace).await?;
    Ok(())
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
        // 杂散文件 (非目录) 也应被删除而不是让 prune 报错 (对齐 TS fs.rm force 语义)
        fs::write(skills.join("stray-file.md"), "x").await.unwrap();

        let (removed, kept_dynamic) = prune_agent_skills(&skills, &["keep-me".to_string()])
            .await
            .unwrap();

        assert!(removed.contains(&"delete-me".to_string()));
        assert!(removed.contains(&"stray-file.md".to_string()));
        assert!(!removed.contains(&"dynamic-me".to_string()));
        assert!(kept_dynamic.contains(&"dynamic-me".to_string()));
        assert!(skills.join("keep-me").exists());
        assert!(!skills.join("delete-me").exists());
        assert!(!skills.join("stray-file.md").exists());
        assert!(skills.join("dynamic-me").exists());
    }

    #[test]
    fn agent_skill_exists_checks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("my-skill")).unwrap();
        assert!(agent_skill_exists(tmp.path(), "my-skill"));
        assert!(!agent_skill_exists(tmp.path(), "nope"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_dir_symlink_creates_relative_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store").join("skills");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("SKILL.md"), "content").await.unwrap();

        let link = tmp.path().join("ws").join(".agents").join("skills");

        force_dir_symlink(&link, &target).await.unwrap();

        assert!(link.is_symlink(), "link should be a symlink on unix");
        assert_eq!(
            fs::read_to_string(link.join("SKILL.md")).await.unwrap(),
            "content"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn link_workspace_creates_all_agent_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("session");

        let store_skills = tmp.path().join("store").join("skills");
        let store_agents = tmp.path().join("store").join("agents");
        fs::create_dir_all(&store_skills).await.unwrap();
        fs::create_dir_all(&store_agents).await.unwrap();
        fs::write(store_skills.join("a.md"), "a").await.unwrap();
        fs::write(store_agents.join("b.md"), "b").await.unwrap();

        link_workspace_to_agent_store(&workspace, &store_skills, &store_agents)
            .await
            .unwrap();

        for dir in crate::service::skills::ALL_AGENT_DIRS {
            let s = workspace.join(dir).join("skills");
            let a = workspace.join(dir).join("agents");
            assert!(s.is_symlink(), "{dir}/skills should be symlink");
            assert!(a.is_symlink(), "{dir}/agents should be symlink");
            assert!(s.join("a.md").exists(), "{dir}/skills/a.md should exist");
            assert!(a.join("b.md").exists(), "{dir}/agents/b.md should exist");
        }
    }
}

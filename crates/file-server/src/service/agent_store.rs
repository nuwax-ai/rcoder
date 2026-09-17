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

/// F01 数据保护：skill/subagent 名、agent ID、项目 ID 在 store/共享视图中只允许
/// 作为**单一路径段**参与 join——含分隔符、`.`/`..` 点段或绝对前缀的名字会把
/// 删除/写入/链接目标解析到受管目录之外（如清单项 `../../victim` 在视图校准
/// "实体缺失清孤儿链"分支触发工作区外的递归删除）。
///
/// 判定用 [`std::path::Path::components`] 的平台原生归一化：恰好一个
/// [`std::path::Component::Normal`] 才放行——`.` 被归一成 CurDir、`..` 成
/// ParentDir、前导 `/` 成 RootDir、Windows 下 `\` 分隔与 `C:` 盘符各成对应
/// Component，全部不满足"单 Normal 段"（容器内 Linux 运行时 `a\b`、`a:b`
/// 是合法单段文件名，不误伤；NUL 在任何平台都不是合法路径字节，显式拒绝）。
/// 所有写/prune/链接入口必须先过本校验；持久 manifest 读回同样校验。
pub fn validate_store_segment(kind: &str, value: &str) -> AppResult<()> {
    let mut components = Path::new(value).components();
    // 规范形态不变式（Q01）：唯一 Normal 段必须与输入字符串完全相等，且
    // 输入不得会被 trim 改变（`" .."` 裸串是合法单段，但任何下游 trim 都会
    // 把它变成 `..`——"校验一种值、使用另一种值"一律拒绝）。后续
    // normalize_names 的 trim 对已过检值是恒等变换。
    let canonical_single_segment = matches!(
        components.next(),
        Some(std::path::Component::Normal(os)) if os == value
    ) && components.next().is_none();
    if !canonical_single_segment || value.contains('\0') || value.trim() != value {
        return Err(crate::error::AppError::validation(format!(
            "invalid {kind} {value:?}: must be a single normalized path segment \
             (no separators, dot segments, surrounding whitespace, or absolute prefixes)"
        )));
    }
    Ok(())
}

/// V05：受管 ACP 根身份核验——`.agents/.claude/.opencode/.codex/.grok/.pi`
/// 全部根在任何 manifest 写入/create/prune **之前**校验：任何根（或将创建
/// 其子目录的祖先）是指向受管树之外的符号链接时，后续 create_dir_all/
/// read_dir/递归删除会沿链接作用于目标目录。查询错误不得当不存在
/// （fail closed：NotFound=尚无根，合法；其他错误=不可判定，拒绝）。
pub async fn validate_managed_roots(workspace: &Path) -> AppResult<()> {
    for dir in crate::service::skills::ALL_AGENT_DIRS {
        let root = workspace.join(dir);
        match fs::symlink_metadata(&root).await {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(crate::error::AppError::validation(format!(
                    "managed agent directory is unreadable: {} ({e})",
                    root.display()
                )));
            }
            Ok(meta) if meta.is_symlink() => {
                return Err(crate::error::AppError::validation(format!(
                    "managed agent directory is a symlink: {} (refusing to operate \
                     through a replaced root)",
                    root.display()
                )));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// 规范化 + 校验清单（Q01）：先 trim/去重，再校验**实际落盘与使用**的值——
/// 原串（如 `" .."`）合法不代表 trim 后（`..`）合法；全程只使用校验后的值。
fn canonicalize_names(kind: &str, names: Vec<String>) -> AppResult<Vec<String>> {
    let canonical = normalize_names(names);
    for name in &canonical {
        validate_store_segment(kind, name)?;
    }
    Ok(canonical)
}

/// 智能体级实体存储路径: `{user_root}/.agent-store/{agent_id}`。
///
/// `user_root` = 会话工作区的父目录 (该 user 的稳定根), 两种 resolver 模式下均成立:
/// - Local: `{COMPUTER_WORKSPACE_DIR}/{userId}` (对齐 TS `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/...`
///   ——normalProject 与 taskAgent 共用同一实体子树，TS 1.4.5 同款)
/// - Subvolume (per-user PVC): `{cephfs-root}/{subvolumePath}` (user_id 已被 PVC 吸收)
///
/// ⚠️ TS 6ab47b7 起 userapp 的 store 布局变为随工作区就近
/// （`{workspacePath}/.agent-store/{agentId}`，默认 `{UWS}/.agent-store/{appId}/{agentId}`
/// 含 appId 段）——Rust 侧 userapp 的 store 消费链现状不可达（userapp 直转
/// 恒 legacy、computer 拦截路径被 `is_dir_link` 门控首推必 legacy），userapp
/// 布局与该侧 manifest 视图暂缓（有意偏离）；normalProject 的 manifest 并集
/// 视图已落地（见下方「共享技能视图」段）。
///
/// store 与会话工作区 (`{user_root}/{cId}`) 同属一棵树, 相对软链可跨节点解析。
/// ⚠️ 不要从工作区叶子路径倒推多级父目录 — subvolumePath 深度不定。
pub fn agent_store_path(user_root: &Path, agent_id: &str) -> AppResult<PathBuf> {
    validate_store_segment("agent id", agent_id)?;
    Ok(user_root.join(".agent-store").join(agent_id))
}

/// 跨 agent store 链接冲突检测（共享工作区防线，见 [`link_workspace_to_agent_store`]）。
///
/// 工作区 agent 目录的 store 链是**目录级**整体链接——若现有链已指向另一
/// agentId 的实体子树，重链会让先驻 agent 的技能被静默覆盖。共享工作区
/// （normalProject）已改用 manifest 并集视图（[`sync_shared_skill_view`]），
/// 本防线仅对非共享（taskAgent 每会话独占工作区）生效：仅当能解析出
/// `.agent-store/{owner}` 且 `owner != agent_id` 时拒绝；同 agent 重入幂等
/// 放行，非本机制建立/不可解析的链接不误伤。
pub async fn detect_cross_agent_link_conflict(workspace: &Path, agent_id: &str) -> AppResult<()> {
    let link = workspace.join(".agents").join("skills");
    if !is_dir_link(&link) {
        return Ok(());
    }
    // Unix 相对链 / Windows junction 绝对目标均可读；不可读（异形链接）放行
    let Ok(target) = fs::read_link(&link).await else {
        return Ok(());
    };
    let mut components = target.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == ".agent-store"
            && let Some(owner) = components.next()
        {
            let owner = owner.as_os_str().to_string_lossy();
            if owner != agent_id {
                return Err(crate::error::AppError::validation(format!(
                    "agent skill store conflict: workspace is linked to agent '{owner}', \
                     refusing to relink to agent '{agent_id}' (shared-workspace multi-agent \
                     skill view is not yet supported)"
                )));
            }
            return Ok(());
        }
    }
    Ok(())
}

/// 创建实体目录 `{agent_store}/{skills,agents}`, 返回两个子目录路径。
pub async fn ensure_agent_store_dirs(
    user_root: &Path,
    agent_id: &str,
) -> AppResult<(PathBuf, PathBuf)> {
    let store = agent_store_path(user_root, agent_id)?;
    let skills_dir = store.join("skills");
    let agents_dir = store.join("agents");
    fs::create_dir_all(&skills_dir).await?;
    fs::create_dir_all(&agents_dir).await?;
    Ok((skills_dir, agents_dir))
}

/// 检查 skill 目录是否有 `.dynamic_add.lock`。
async fn has_dynamic_add_lock(skill_path: &Path) -> bool {
    fs::try_exists(skill_path.join(DYNAMIC_ADD_LOCK))
        .await
        .unwrap_or(false)
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

    if !fs::try_exists(skills_dir).await.unwrap_or(false) {
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
        if has_dynamic_add_lock(&skill_path).await {
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
    // F01：名字先校验再 join——`../..` 会把"删旧目标"的 remove_dir_all
    // 解析到 store 之外
    validate_store_segment("skill name", skill_name)?;
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

/// 逐个子目录/文件并发覆盖更新 agents 目录 (无锁安全)。
///
/// 每个条目独立操作: 删旧 → rename 移入。不同条目天然无冲突;
/// 同名条目并发覆盖最终一致。目标目录始终存在, 无"目录空"窗口。
/// F04：视图条目混合目录（多文件 subagent 包）与文件（.md）——删旧按
/// symlink_metadata 分派（remove_dir_all 对文件 ENOTDIR）；**显式给出的源
/// 丢失必须失败**（上传源被提前清理 = 假成功），仅 None = 无输入跳过。
pub async fn update_agents_dir(src: Option<&Path>, dest: &Path) -> AppResult<()> {
    fs::create_dir_all(dest).await?;
    let Some(src) = src else {
        return Ok(());
    };
    // try_exists 而非阻塞的 Path::exists()（本函数在 async 上下文）。
    if !fs::try_exists(src).await.unwrap_or(false) {
        return Err(crate::error::AppError::validation(format!(
            "uploaded agents source disappeared before it could be consumed: {}",
            src.display()
        )));
    }
    let mut entries = fs::read_dir(src).await?;
    let mut tasks = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let dst = dest.join(entry.file_name());
        let src_entry = entry.path();
        tasks.push(async move {
            match fs::symlink_metadata(&dst).await {
                Ok(meta) if meta.is_dir() && !meta.is_symlink() => {
                    fs::remove_dir_all(&dst).await?;
                }
                Ok(_) => {
                    fs::remove_file(&dst).await?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            move_or_copy_directory(&src_entry, &dst).await
        });
    }
    try_join_all(tasks).await?;
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

/// 删除任意形态路径（实体目录/文件/符号链接/junction），NotFound 安全。
/// 视图条目混合目录（skills）与文件（subagents .md），`remove_dir_all` 对
/// 文件会 ENOTDIR——按 symlink_metadata 分派；symlink 只删链不触目标。
async fn remove_any_if_exists(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path).await {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            if meta.is_dir() && !meta.is_symlink() {
                fs::remove_dir_all(path).await.map_err(Into::into)
            } else {
                fs::remove_file(path).await.map_err(Into::into)
            }
        }
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

    // Q02/V05：全部受管根前置核验（不逐循环检查——前序循环迭代已可能写入）
    validate_managed_roots(workspace).await?;

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

    if fs::try_exists(agent_skills_dir).await.unwrap_or(false) {
        crate::service::fs_util::copy_dir_filtered(agent_skills_dir, &primary_skills, &[], &[])
            .await?;
    }
    if fs::try_exists(agent_agents_dir).await.unwrap_or(false) {
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

// ===== 共享技能视图（manifest 并集，对齐 TS 6ab47b7）=====
//
// normalProject 共享工作区（多会话/多智能体并存同一项目目录）的技能视图
// 由项目级 manifest.json 引用表管理：视图 = manifest 引用集（每名字一条链，
// 指向最新来源）∪ 动态锁技能；引用归零才允许从视图移除（实体清理仍由各
// agent 自己的 prune 负责）。
//
// 有意偏离（记录）：
// - 仅 normalProject 激活（TS 还覆盖 userapp）；Rust 侧 userapp 的 store
//   消费链现状不可达（userapp 直转恒 legacy），保持 fail-fast 防线。
// - manifest 损坏（存在但不可解析）报错而非按空表处理——空表会把其他
//   agent 的引用全部清出视图，吞错风险大于重建成本（Fail Fast）。

const MANIFEST_FILE: &str = "manifest.json";
const VIEW_LOCK_NAME: &str = ".view.lock";
const VIEW_LOCK_STALE_MS: u64 = 5 * 60 * 1000;
const VIEW_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const VIEW_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(200);

/// 项目级协调目录（normalProject）：`{user_root}/.agent-store/np-{project_id}`。
/// 仅存放 manifest.json 引用表与 .view.lock 视图锁，不存技能实体——同一
/// agent 的实体全局一份（`agent_store_path`），跨项目复用。
pub fn project_store_root(user_root: &Path, project_id: &str) -> AppResult<PathBuf> {
    validate_store_segment("project id", project_id)?;
    Ok(user_root
        .join(".agent-store")
        .join(format!("np-{project_id}")))
}

/// manifest 引用表：agents → {skills, subagents}。
/// `BTreeMap` 保证键序确定（TS 对象键序），快照/比对可复现。
#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone, PartialEq)]
pub struct SkillViewManifest {
    #[serde(default)]
    agents: std::collections::BTreeMap<String, SkillViewEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone, PartialEq)]
pub struct SkillViewEntry {
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    subagents: Vec<String>,
}

/// 读 manifest；文件不存在 → 空表（新项目）；存在但损坏 → 报错（见模块注释）。
/// 解析成功后校验所有 agent ID 与名字为合法单路径段（F01）——被污染的
/// manifest 名字会在视图校准中被 join 后删除/链接，读回时必须拒绝。
async fn read_manifest(project_store_root: &Path) -> AppResult<SkillViewManifest> {
    let path = project_store_root.join(MANIFEST_FILE);
    let raw = match fs::read_to_string(&path).await {
        Ok(raw) if raw.trim().is_empty() => return Ok(SkillViewManifest::default()),
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(SkillViewManifest::default()),
        Err(e) => return Err(e.into()),
    };
    let manifest: SkillViewManifest = serde_json::from_str(&raw).map_err(|error| {
        crate::error::AppError::system(format!(
            "skill view manifest is corrupt at {}: {error}",
            path.display()
        ))
    })?;
    for (agent_id, entry) in &manifest.agents {
        validate_store_segment("agent id", agent_id).map_err(|_| {
            crate::error::AppError::system(format!(
                "skill view manifest at {} contains an invalid agent id",
                path.display()
            ))
        })?;
        for name in entry.skills.iter().chain(entry.subagents.iter()) {
            validate_store_segment("skill name", name).map_err(|_| {
                crate::error::AppError::system(format!(
                    "skill view manifest at {} contains an invalid entry name",
                    path.display()
                ))
            })?;
        }
    }
    Ok(manifest)
}

/// 原子写 manifest（tmp + rename），视图锁内调用。
async fn write_manifest(project_store_root: &Path, manifest: &SkillViewManifest) -> AppResult<()> {
    fs::create_dir_all(project_store_root).await?;
    let tmp = project_store_root.join(format!(
        "{MANIFEST_FILE}.{}.{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let body = serde_json::to_string_pretty(manifest)
        .map_err(|e| crate::error::AppError::system(e.to_string()))?;
    fs::write(&tmp, body).await?;
    if let Err(e) = fs::rename(&tmp, project_store_root.join(MANIFEST_FILE)).await {
        fs::remove_file(&tmp).await.ok();
        return Err(e.into());
    }
    Ok(())
}

/// 视图种类：store 子目录 / 挂载子目录 / manifest 字段三映射。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewKind {
    Skills,
    Subagents,
}

impl ViewKind {
    const ALL: [ViewKind; 2] = [ViewKind::Skills, ViewKind::Subagents];
    fn store_sub(&self) -> &'static str {
        match self {
            ViewKind::Skills => "skills",
            ViewKind::Subagents => "agents",
        }
    }
    fn mount_sub(&self) -> &'static str {
        match self {
            ViewKind::Skills => "skills",
            ViewKind::Subagents => "agents",
        }
    }
    fn manifest_field<'a>(&self, entry: &'a SkillViewEntry) -> &'a [String] {
        match self {
            ViewKind::Skills => &entry.skills,
            ViewKind::Subagents => &entry.subagents,
        }
    }
    fn manifest_field_mut<'a>(&self, entry: &'a mut SkillViewEntry) -> &'a mut Vec<String> {
        match self {
            ViewKind::Skills => &mut entry.skills,
            ViewKind::Subagents => &mut entry.subagents,
        }
    }
}

/// 反向引用：name → 引用它的 agentId 列表（manifest 键序）。
fn reverse_refs(
    manifest: &SkillViewManifest,
    kind: ViewKind,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut refs = std::collections::BTreeMap::new();
    for (agent_id, entry) in &manifest.agents {
        for name in kind.manifest_field(entry) {
            refs.entry(name.clone())
                .or_insert_with(Vec::new)
                .push(agent_id.clone());
        }
    }
    refs
}

/// 视图锁（项目级，O_EXCL 创建 + 过期自愈 + **所有权保护**）。
///
/// Q07 三项修复：
/// 1. 锁文件内容 = 持有者 token——Drop 只在内容仍是**自己的** token 时删除：
///    A（被 B 判定过期接管后）的迟到 Drop 不再删掉 B 的锁放 C 进来；
/// 2. stale 判定只认 mtime 超龄——metadata **读取错误不当 stale**（IO 抖动
///    不是"无主"证明，按忙等重试）；
/// 3. 接管 = 校验 token 未变后 write-temp + **rename** 原子替换（同 inode
///    域内原子；B 接管与 A 迟到释放竞争时，rename 后 A 的 remove 命中的
///    token 校验必然失败——不再有"删掉别人锁"窗口）。
struct ViewGuard {
    path: PathBuf,
    token: String,
}

impl ViewGuard {
    async fn acquire(project_store_root: &Path) -> AppResult<Self> {
        let lock = project_store_root.join(VIEW_LOCK_NAME);
        let token = uuid::Uuid::new_v4().to_string();
        let deadline = std::time::Instant::now() + VIEW_LOCK_WAIT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
                .await
            {
                Ok(mut file) => {
                    use tokio::io::AsyncWriteExt as _;
                    file.write_all(token.as_bytes()).await?;
                    // 锁内容必须在持有者返回前可靠落盘：显式 sync 消除
                    // 写入延迟可见（接管方读到空 token 的窗口）
                    file.sync_all().await?;
                    return Ok(ViewGuard { path: lock, token });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    // 过期自愈（5 分钟）。Q07：metadata 读取失败 ≠ stale——
                    // 元数据不可读时按"仍被持有"处理（保守等待），只有明确的
                    // mtime 超龄才构成接管依据。
                    let stale = match fs::metadata(&lock).await {
                        Ok(meta) => meta
                            .modified()
                            .ok()
                            .and_then(|at| at.elapsed().ok())
                            .is_some_and(|age| age.as_millis() as u64 > VIEW_LOCK_STALE_MS),
                        Err(e) if e.kind() == ErrorKind::NotFound => true,
                        Err(_) => false,
                    };
                    if stale {
                        // 接管：读旧 token → 写临时文件（自己的 token）→
                        // 校验旧 token 未变 → rename 原子替换。校验失败说明
                        // 别人刚接管/释放，回锁竞争循环。
                        let observed = fs::read_to_string(&lock).await.unwrap_or_default();
                        let candidate = lock.with_extension(format!("takeover-{token}"));
                        fs::write(&candidate, &token).await?;
                        let current = fs::read_to_string(&lock).await.unwrap_or_default();
                        if current == observed {
                            match fs::rename(&candidate, &lock).await {
                                Ok(()) => return Ok(ViewGuard { path: lock, token }),
                                Err(e) if e.kind() == ErrorKind::NotFound => {
                                    // 锁在接管窗口被持有人正常释放——重试创建
                                    fs::remove_file(&candidate).await.ok();
                                }
                                Err(e) => {
                                    fs::remove_file(&candidate).await.ok();
                                    return Err(e.into());
                                }
                            }
                        } else {
                            fs::remove_file(&candidate).await.ok();
                        }
                        continue;
                    }
                }
                Err(e) => return Err(e.into()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(crate::error::AppError::system(
                    "agent skill view is busy, please retry",
                ));
            }
            tokio::time::sleep(VIEW_LOCK_RETRY).await;
        }
    }
}

impl Drop for ViewGuard {
    fn drop(&mut self) {
        // Q07：只释放**自己的**锁——内容仍是本持有者 token 才删除；否则
        // 锁已被接管（或他人持有），迟到 Drop 不得误删
        let path = &self.path;
        let token = self.token.clone();
        if let Ok(content) = std::fs::read_to_string(path)
            && content == token
        {
            std::fs::remove_file(path).ok();
        }
    }
}

/// 本次同步要写入 manifest 的清单；None = 不更新该字段（仅校准视图，
/// 防旧客户端未传清单时空集误清引用——TS 同款语义）。
#[derive(Default)]
pub struct SharedSkillLists {
    pub skills: Option<Vec<String>>,
    pub subagents: Option<Vec<String>>,
}

/// 共享工作区技能视图同步（manifest 驱动、链级增量、无整体重建）。
///
/// 1. 视图锁内更新 manifest：本 agent 的 skills/subagents = 本次清单；
/// 2. 挂载点：`.agents/{skills,agents}` 为实体主目录（清掉遗留目录级旧链），
///    其余 ACP 目录同位子目录为指向主目录的内链（不可用时复制兜底）；
/// 3. 校准式同步：删除无引用且非动态的条目；引用条目按「当前 agent 优先、
///    实体存在者优先」重指（崩溃自愈）；引用存在但实体全缺 → 清孤儿链；
/// 4. 动态技能（带 `.dynamic_add.lock`）并入并集：为本 agent 及 manifest
///    各 agent 实体子树中的动态技能补缺失链。
pub async fn sync_shared_skill_view(
    user_root: &Path,
    workspace: &Path,
    agent_id: &str,
    project_id: &str,
    lists: SharedSkillLists,
) -> AppResult<()> {
    // F01/Q01：先规范化再校验，且只把**校验后的值**写 manifest 与用于视图
    // 操作——校验原始串、使用 trim 后的值会让 `" .."` 这类输入把 `..` 落盘
    // 并 join 成受管范围外的删除/链接目标
    let skills = lists
        .skills
        .map(|names| canonicalize_names("skill name", names))
        .transpose()?;
    let subagents = lists
        .subagents
        .map(|names| canonicalize_names("subagent name", names))
        .transpose()?;
    // Q02/V05：全部受管 ACP 根在任何写/链接操作前核验（见帮助函数注释）。
    // 合法 ACP 视图**条目**链接（.agents/skills/<name> → store 实体）不受影响。
    validate_managed_roots(workspace).await?;
    let store_root = project_store_root(user_root, project_id)?;
    fs::create_dir_all(&store_root).await?;
    let _guard = ViewGuard::acquire(&store_root).await?;

    let mut manifest = read_manifest(&store_root).await?;
    let entry = manifest.agents.entry(agent_id.to_string()).or_default();
    if let Some(skills) = skills {
        *ViewKind::Skills.manifest_field_mut(entry) = skills;
    }
    if let Some(subagents) = subagents {
        *ViewKind::Subagents.manifest_field_mut(entry) = subagents;
    }
    write_manifest(&store_root, &manifest).await?;

    // 挂载点：.agents 实体主目录 + 其余 ACP 目录内链（一份实体四目录复用）
    let mut copy_fallback: Vec<(PathBuf, PathBuf)> = Vec::new();
    for dir in crate::service::skills::ALL_AGENT_DIRS {
        for sub in ["skills", "agents"] {
            let sub_path = workspace.join(dir).join(sub);
            if let Some(parent) = sub_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            if *dir == ".agents" {
                // 旧结构的目录级软链（指向单 agent store）不留存——换实体目录
                if is_dir_link(&sub_path) {
                    remove_any_if_exists(&sub_path).await?;
                }
                fs::create_dir_all(&sub_path).await?;
            } else {
                let primary_sub = workspace.join(".agents").join(sub);
                let linked_ok = is_dir_link(&sub_path)
                    && match fs::read_link(&sub_path).await {
                        Ok(target) => sub_path
                            .parent()
                            .and_then(|parent| pathdiff::diff_paths(&primary_sub, parent))
                            .is_some_and(|expected| target == expected),
                        Err(_) => false,
                    };
                if !linked_ok {
                    remove_any_if_exists(&sub_path).await?;
                    if force_dir_symlink(&sub_path, &primary_sub).await.is_err() {
                        copy_fallback.push((primary_sub, sub_path));
                    }
                }
            }
        }
    }

    // 校准式同步 + 动态补链
    for kind in ViewKind::ALL {
        calibrate_view_entries(user_root, workspace, agent_id, &manifest, kind).await?;
        if kind == ViewKind::Skills {
            link_dynamic_skills(user_root, workspace, agent_id, &manifest).await?;
        }
    }

    // 复制兜底的内链：主目录（并集）填充完成后整体复制（解引用快照语义）
    for (primary_sub, sub_path) in copy_fallback {
        remove_any_if_exists(&sub_path).await?;
        crate::service::fs_util::copy_dir_filtered(&primary_sub, &sub_path, &[], &[]).await?;
    }

    tracing::info!(
        op = "sync_shared_skill_view",
        project_id,
        agent_id,
        agents = manifest.agents.len(),
        "shared skill view synced"
    );
    Ok(())
}

/// 清单归一：trim、去空、去重（保序）。
fn normalize_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// 校准视图条目：删无引用非动态 → 引用条目按来源优先级补链/重指 → 清孤儿链。
async fn calibrate_view_entries(
    user_root: &Path,
    workspace: &Path,
    agent_id: &str,
    manifest: &SkillViewManifest,
    kind: ViewKind,
) -> AppResult<()> {
    let mount_dir = workspace.join(".agents").join(kind.mount_sub());
    let refs = reverse_refs(manifest, kind);

    // 删除：不在引用集且非动态（动态条目由补链循环维护）
    if fs::try_exists(&mount_dir).await.unwrap_or(false) {
        let mut rd = fs::read_dir(&mount_dir).await?;
        let mut existing = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            existing.push(entry.file_name());
        }
        for name in existing {
            let link_path = mount_dir.join(&name);
            let name = name.to_string_lossy().to_string();
            if name == DYNAMIC_ADD_LOCK || has_dynamic_add_lock(&link_path).await {
                continue;
            }
            if refs.contains_key(&name) {
                continue;
            }
            remove_any_if_exists(&link_path).await?;
            tracing::info!(kind = ?kind, name, "skill view entry removed (no agent references it)");
        }
    }

    // 引用条目：当前 agent 优先、实体存在者优先；全部缺失清孤儿链
    for (name, sources) in &refs {
        let link_path = mount_dir.join(name);
        if has_dynamic_add_lock(&link_path).await {
            continue;
        }
        let mut candidates: Vec<String> = vec![agent_id.to_string()];
        candidates.extend(sources.iter().cloned());
        let mut seen = std::collections::BTreeSet::new();
        let mut store_path: Option<PathBuf> = None;
        for candidate in candidates
            .into_iter()
            .filter(|candidate| seen.insert(candidate.clone()))
        {
            // agent_store_path 内含单路径段校验（F01）；manifest/入口已校验过，
            // 这里失败即数据异常，按错误上抛而非静默跳过
            let path = agent_store_path(user_root, &candidate)?
                .join(kind.store_sub())
                .join(name);
            if path.exists() {
                store_path = Some(path);
                break;
            }
        }
        let Some(store_path) = store_path else {
            remove_any_if_exists(&link_path).await?;
            continue;
        };
        // 期望链目标（相对 mount_dir）；已是正确链则不重建（运行中的 agent
        // 不会读到瞬时空目录）
        let need_relink = match fs::read_link(&link_path).await {
            Ok(target) => pathdiff::diff_paths(&store_path, &mount_dir)
                .is_none_or(|expected| target != expected),
            Err(_) => true,
        };
        if need_relink {
            remove_any_if_exists(&link_path).await?;
            install_view_entry(&store_path, &link_path, kind).await?;
        }
    }
    Ok(())
}

/// 动态技能补链：manifest 不管理动态技能，但视图必须并入它们的并集。
async fn link_dynamic_skills(
    user_root: &Path,
    workspace: &Path,
    agent_id: &str,
    manifest: &SkillViewManifest,
) -> AppResult<()> {
    let mount_dir = workspace.join(".agents").join("skills");
    let mut owners: Vec<String> = vec![agent_id.to_string()];
    owners.extend(manifest.agents.keys().cloned());
    let mut seen = std::collections::BTreeSet::new();
    for owner in owners.into_iter().filter(|o| seen.insert(o.clone())) {
        let owner_skills = agent_store_path(user_root, &owner)?.join("skills");
        let mut rd = match fs::read_dir(&owner_skills).await {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        while let Some(entry) = rd.next_entry().await? {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let entity = entry.path();
            if !has_dynamic_add_lock(&entity).await {
                continue;
            }
            let link_path = mount_dir.join(entry.file_name());
            if link_path.exists() {
                continue;
            }
            install_view_entry(&entity, &link_path, ViewKind::Skills).await?;
            tracing::info!(owner, name = %entry.file_name().to_string_lossy(), "dynamic skill linked into view");
        }
    }
    Ok(())
}

/// 视图条目安装：目录建链（失败复制兜底）；文件（subagent .md）建文件链
/// （Unix；Windows 无文件链权限直接复制）。
async fn install_view_entry(store_path: &Path, link_path: &Path, kind: ViewKind) -> AppResult<()> {
    let meta = fs::symlink_metadata(store_path).await?;
    if meta.is_dir() || kind == ViewKind::Skills {
        if force_dir_symlink(link_path, store_path).await.is_err() {
            fs::create_dir_all(link_path).await?;
            crate::service::fs_util::copy_dir_filtered(store_path, link_path, &[], &[]).await?;
        }
        return Ok(());
    }
    // 文件实体：Unix 相对链优先，失败/Windows 复制
    #[cfg(unix)]
    {
        let parent = link_path
            .parent()
            .ok_or_else(|| crate::error::AppError::system("view link path has no parent"))?;
        if let Some(relative) = pathdiff::diff_paths(store_path, parent)
            && fs::symlink(&relative, link_path).await.is_ok()
        {
            return Ok(());
        }
    }
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::copy(store_path, link_path).await?;
    Ok(())
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
    /// 跨 agent store 链接冲突防线（P1 回归锁）：共享工作区先后由 A、B 接管时，
    /// B 的重链会让 A 的技能被静默覆盖——防线在此场景 fail-fast；同 agent 重入
    /// 与无链工作区幂等放行（manifest 并集视图复刻前的过渡防线）。
    #[cfg(unix)]
    #[tokio::test]
    async fn cross_agent_link_conflict_rejected_same_agent_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).await.unwrap();

        // 无链工作区（首次创建）→ 放行
        assert!(
            detect_cross_agent_link_conflict(&ws, "agent-a")
                .await
                .is_ok()
        );

        // 建链到 agent-a 的 store（模拟 A 已创建工作区；链接目标含
        // `.agent-store/{agentId}` 段——本机制建立的形态）
        let store_a = tmp
            .path()
            .join("u1")
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&store_a).await.unwrap();
        let link = ws.join(".agents").join("skills");
        fs::create_dir_all(link.parent().unwrap()).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&store_a, link.parent().unwrap()).unwrap(),
            &link,
        )
        .await
        .unwrap();

        // 同 agent 重入 → 幂等放行
        assert!(
            detect_cross_agent_link_conflict(&ws, "agent-a")
                .await
                .is_ok()
        );

        // 不同 agent 接管 → 拒绝（防静默覆盖）
        let err = detect_cross_agent_link_conflict(&ws, "agent-b")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("agent skill store conflict"), "{msg}");
        assert!(msg.contains("agent-a") && msg.contains("agent-b"), "{msg}");

        // 异形链（目标不含 .agent-store 段，非本机制建立）→ 不误伤放行
        let ws3 = tmp.path().join("ws3");
        fs::create_dir_all(ws3.join(".agents")).await.unwrap();
        let foreign = tmp.path().join("foreign-skills");
        fs::create_dir_all(&foreign).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&foreign, ws3.join(".agents")).unwrap(),
            ws3.join(".agents").join("skills"),
        )
        .await
        .unwrap();
        assert!(
            detect_cross_agent_link_conflict(&ws3, "agent-b")
                .await
                .is_ok(),
            "foreign-shaped link must not be rejected"
        );

        // 相对链解析出的 owner 是 .agent-store 段的直接下一级（store 布局形态）
        let ws2 = tmp.path().join("ws2");
        fs::create_dir_all(ws2.join(".agents")).await.unwrap();
        let user_root = tmp.path().join("root").join("u1");
        let store = user_root
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&store).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&store, ws2.join(".agents")).unwrap(),
            ws2.join(".agents").join("skills"),
        )
        .await
        .unwrap();
        assert!(
            detect_cross_agent_link_conflict(&ws2, "agent-b")
                .await
                .is_err(),
            "store 布局形态的跨 agent 同样拒绝"
        );
        assert!(
            detect_cross_agent_link_conflict(&ws2, "agent-a")
                .await
                .is_ok(),
            "store 布局形态的同 agent 放行"
        );
    }

    // ===== 共享技能视图（manifest 并集）=====

    async fn seed_agent_skill(user_root: &Path, agent_id: &str, name: &str, dynamic: bool) {
        let (skills_dir, _) = ensure_agent_store_dirs(user_root, agent_id).await.unwrap();
        let src = skills_dir.join(name);
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("SKILL.md"), format!("{agent_id}/{name}"))
            .await
            .unwrap();
        if dynamic {
            ensure_dynamic_add_lock(&src).await.unwrap();
        }
    }

    async fn view_entry_names(workspace: &Path, sub: &str) -> Vec<String> {
        let mount = workspace.join(".agents").join(sub);
        let mut names = Vec::new();
        if let Ok(mut rd) = fs::read_dir(&mount).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        names.sort();
        names
    }

    #[tokio::test]
    async fn shared_view_unions_multiple_agents_without_conflict() {
        // 修复前（目录级整链 + 跨 agent 防线）：agent-b 对同一共享工作区会被
        // fail-fast 拒绝；manifest 并集视图下两 agent 技能并存
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        seed_agent_skill(&user_root, "agent-b", "beta", false).await;

        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: Some(vec![]),
            },
        )
        .await
        .unwrap();
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["beta".into()]),
                subagents: Some(vec![]),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha", "beta"],
            "并集视图同时含两 agent 技能"
        );
        // manifest 记录两 agent 引用
        let manifest = read_manifest(&project_store_root(&user_root, "proj").unwrap())
            .await
            .unwrap();
        assert_eq!(manifest.agents.len(), 2);
        assert_eq!(manifest.agents["agent-a"].skills, vec!["alpha".to_string()]);
        assert_eq!(manifest.agents["agent-b"].skills, vec!["beta".to_string()]);
        // 挂载结构：.agents 实体目录 + 其余 ACP 目录内链
        assert!(workspace.join(".agents").join("skills").is_dir());
        for dir in crate::service::skills::ALL_AGENT_DIRS {
            if *dir == ".agents" {
                continue;
            }
            let link = workspace.join(dir).join("skills");
            assert!(is_dir_link(&link), "{} 内链存在", dir);
            assert!(
                fs::read_link(&link)
                    .await
                    .unwrap()
                    .ends_with(".agents/skills"),
                "{dir} 内链指向主目录"
            );
        }
    }

    #[tokio::test]
    async fn shared_view_removes_entry_only_when_refs_reach_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        seed_agent_skill(&user_root, "agent-b", "alpha", false).await;

        for agent in ["agent-a", "agent-b"] {
            sync_shared_skill_view(
                &user_root,
                &workspace,
                agent,
                "proj",
                SharedSkillLists {
                    skills: Some(vec!["alpha".into()]),
                    subagents: None,
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(view_entry_names(&workspace, "skills").await, vec!["alpha"]);

        // agent-a 引用归零：agent-b 仍引用 → 保留
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha"],
            "仍有 agent 引用时保留"
        );

        // agent-b 也归零 → 移除
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert!(
            view_entry_names(&workspace, "skills").await.is_empty(),
            "引用全部归零后移除"
        );
    }

    #[tokio::test]
    async fn shared_view_keeps_dynamic_skills_outside_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "configured", false).await;
        seed_agent_skill(&user_root, "agent-a", "dyn-skill", true).await;

        // 动态技能不进 manifest（清单只写 configured）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["configured".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["configured", "dyn-skill"],
            "动态技能并入并集"
        );

        // 清单清空（校准模式）也不清动态条目
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["dyn-skill"],
            "动态条目不受校准影响"
        );

        // 其他 agent 的同步也不清它（动态锁保护跨 agent 校准）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["dyn-skill"]
        );
    }

    #[tokio::test]
    async fn shared_view_push_mode_without_lists_only_calibrates() {
        // push-skills 自愈模式（清单 None）：不更新 manifest、只校准视图
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        // agent-b 不带清单同步：其引用不写入（不会把引用集改写为空）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists::default(),
        )
        .await
        .unwrap();
        let manifest = read_manifest(&project_store_root(&user_root, "proj").unwrap())
            .await
            .unwrap();
        // TS 同款：同步者条目无条件落盘（空清单条目无引用，无害），但引用集
        // 不被改写——alpha 仍由 agent-a 引用，视图不被清空
        assert_eq!(
            manifest.agents["agent-b"],
            SkillViewEntry::default(),
            "未传清单时新增条目为空（不产生引用）"
        );
        assert_eq!(
            manifest.agents["agent-a"].skills,
            vec!["alpha".to_string()],
            "既有引用未被改写"
        );
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha"],
            "引用集未被清空"
        );
    }

    #[tokio::test]
    async fn shared_view_corrupt_manifest_fails_fast() {
        // 有意偏离 TS（catch → 空表）：损坏即报错——空表会把其他 agent 的
        // 引用全部清出视图
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let store_root = project_store_root(&user_root, "proj").unwrap();
        fs::create_dir_all(&store_root).await.unwrap();
        fs::write(store_root.join(MANIFEST_FILE), "{ not json")
            .await
            .unwrap();
        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await;
        assert!(result.is_err(), "损坏 manifest 必须 fail-fast");
    }

    #[tokio::test]
    async fn shared_view_clears_orphan_links_when_entities_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        // 实体被外部删除（各 agent prune 已清）；引用还在 → 孤儿链清理
        fs::remove_dir_all(
            agent_store_path(&user_root, "agent-a")
                .unwrap()
                .join("skills/alpha"),
        )
        .await
        .unwrap();
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await
        .unwrap();
        assert!(
            view_entry_names(&workspace, "skills").await.is_empty(),
            "实体全缺时清孤儿链"
        );
    }

    // ===== F01 数据保护回归：清单名/ID 路径穿越必须在任何写/删除前被拒绝 =====

    #[test]
    fn validate_store_segment_rejects_traversal_and_accepts_normal_names() {
        // 平台原生 components() 判定 + 规范形态不变式（Q01）：唯一 Normal 段
        // 必须与输入完全相等——会被归一化改变形态的输入（"a/"、"a/."、前后
        // 空白包围点段）一律拒绝，杜绝"校验一种值、使用另一种值"。
        for bad in [
            "",
            ".",
            "..",
            " ..",
            ".. ",
            " . ",
            "./a",
            "/absolute",
            "a/",
            "a/.",
            "a/b",
            "../../victim",
            "a\0b",
        ] {
            assert!(
                validate_store_segment("skill name", bad).is_err(),
                "must reject {bad:?}"
            );
        }
        // Windows 分隔符/盘符在 win 构建下由 components() 识别为多段/前缀；
        // 容器内 Linux 运行时它们是合法单段文件名，不放进来回测试
        #[cfg(windows)]
        for bad in ["a\\b", "..\\..\\victim", "C:evil"] {
            assert!(
                validate_store_segment("skill name", bad).is_err(),
                "must reject {bad:?} on Windows"
            );
        }
        for ok in ["alpha", "my-skill_1.2", "技能", "sub agent name"] {
            assert!(
                validate_store_segment("skill name", ok).is_ok(),
                "must accept {ok:?}"
            );
        }
    }

    #[tokio::test]
    async fn install_skill_dir_rejects_escaping_name_without_touching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        // 受害目录在 store 同层（= dest_skills_dir/../../victim 解析目标）
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).await.unwrap();
        fs::write(victim.join("data.txt"), "keep").await.unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).await.unwrap();
        let skills_dir = user_root
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&skills_dir).await.unwrap();

        let result = install_skill_dir(&src, &skills_dir, "../../victim", false).await;
        assert!(result.is_err(), "escaping skill name must be rejected");
        assert_eq!(
            fs::read_to_string(victim.join("data.txt")).await.unwrap(),
            "keep",
            "受害者目录内容必须原样保留"
        );
    }

    #[tokio::test]
    async fn poisoned_manifest_name_is_rejected_and_deletes_nothing() {
        // 恶意/损坏清单：名字为 `../../victim`——修复前会在视图校准
        // "实体缺失清孤儿链"分支把它 join 后递归删除工作区外目录
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).await.unwrap();
        fs::write(victim.join("data.txt"), "keep").await.unwrap();

        let store_root = project_store_root(&user_root, "proj").unwrap();
        fs::create_dir_all(&store_root).await.unwrap();
        fs::write(
            store_root.join(MANIFEST_FILE),
            r#"{"agents":{"agent-a":{"skills":["../../victim"],"subagents":[]}}}"#,
        )
        .await
        .unwrap();

        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await;
        assert!(result.is_err(), "被污染 manifest 必须整体拒绝");
        assert_eq!(
            fs::read_to_string(victim.join("data.txt")).await.unwrap(),
            "keep",
            "受管范围外目录必须原样保留"
        );
    }

    #[tokio::test]
    async fn view_lock_takeover_and_late_release_never_delete_successor_lock() {
        // Q07：A 持锁（伪造超龄 mtime）→ B 接管（token 轮换）→ A 的迟到
        // Drop 不得删掉 B 的锁（C 不得凭空进入）；B 正常释放有效
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj-store");
        fs::create_dir_all(&root).await.unwrap();
        let lock = root.join(VIEW_LOCK_NAME);
        // A 持锁
        let a = ViewGuard::acquire(&root).await.expect("A acquires");
        // 伪造超龄：直接回写 mtime（文件锁内容为 A token）
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(VIEW_LOCK_STALE_MS + 60);
        let f = std::fs::File::options().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        // B 接管（stale 检测 → token 轮换 rename）
        let b = ViewGuard::acquire(&root).await.expect("B takes over");
        assert_ne!(a.token, b.token, "接管必须轮换所有权 token");
        // A 迟到 Drop：锁内容是 B 的 token——不得删除
        drop(a);
        assert!(lock.exists(), "迟到 Drop 不得删除接管者的锁（Q07）");
        assert_eq!(
            fs::read_to_string(&lock).await.unwrap(),
            b.token,
            "锁内容必须仍是 B 的所有权 token"
        );
        // B 正常释放有效
        drop(b);
        assert!(!lock.exists(), "持有者自身释放必须生效");
    }

    #[tokio::test]
    async fn trimmed_dot_segments_rejected_before_manifest_write() {
        // Q01：原始串（".. "）是合法 Normal 组件，但 trim 后变成 ".."——
        // 规范化后的值必须在写 manifest 前被拒，不能把 ".." 落盘参与视图操作
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![".. ".to_string(), "normal".to_string()]),
                subagents: None,
            },
        )
        .await;
        assert!(result.is_err(), "trim 后非法的名字必须整单拒绝");
        let store_root = project_store_root(&user_root, "proj").unwrap();
        let manifest = read_manifest(&store_root).await.unwrap();
        assert!(
            manifest.agents.is_empty(),
            "非法清单不得写入 manifest: {manifest:?}"
        );
    }

    #[tokio::test]
    async fn replaced_non_agents_acp_roots_are_refused_across_all_variants() {
        // V05：.agents 之外的 ACP 根（.claude/.opencode/.codex/.grok/.pi 任一）
        // 被替换为指向 victim 的链接——sync 与 link 两条路径都必须拒绝，
        // 且 victim、manifest、其余视图零变更
        for replaced in [".claude", ".opencode", ".codex", ".grok", ".pi"] {
            let tmp = tempfile::tempdir().unwrap();
            let user_root = tmp.path().join("u1");
            let workspace = tmp.path().join("ws");
            fs::create_dir_all(&workspace).await.unwrap();
            let victim = tmp.path().join("victim");
            fs::create_dir_all(victim.join("skills").join("keep"))
                .await
                .unwrap();
            fs::write(victim.join("skills").join("keep").join("SKILL.md"), "keep")
                .await
                .unwrap();
            // 受害目录挂在被替换的根上
            #[cfg(unix)]
            std::os::unix::fs::symlink(&victim, workspace.join(replaced)).unwrap();

            let sync_result = sync_shared_skill_view(
                &user_root,
                &workspace,
                "agent-a",
                "proj",
                SharedSkillLists {
                    skills: Some(Vec::new()),
                    subagents: None,
                },
            )
            .await;
            let link_result = link_workspace_to_agent_store(
                &workspace,
                &user_root
                    .join(".agent-store")
                    .join("agent-a")
                    .join("skills"),
                &user_root
                    .join(".agent-store")
                    .join("agent-a")
                    .join("agents"),
            )
            .await;
            #[cfg(unix)]
            {
                assert!(
                    sync_result.is_err(),
                    "{replaced} 根被链接替换时 sync 必须拒绝"
                );
                assert!(
                    link_result.is_err(),
                    "{replaced} 根被链接替换时 link 必须拒绝"
                );
                assert_eq!(
                    fs::read_to_string(victim.join("skills").join("keep").join("SKILL.md"))
                        .await
                        .unwrap(),
                    "keep",
                    "{replaced}: victim 内容必须原样保留"
                );
                let store_root = project_store_root(&user_root, "proj").unwrap();
                let manifest = read_manifest(&store_root).await.unwrap();
                assert!(
                    manifest.agents.is_empty(),
                    "{replaced}: 非法清单不得写入 manifest"
                );
            }
            #[cfg(not(unix))]
            let _ = (sync_result, link_result);
        }
    }

    #[tokio::test]
    async fn replaced_agents_root_symlink_is_refused_without_touching_target() {
        // Q02：workspace/.agents 被替换为指向 victim 的链接——同步必须拒绝，
        // 不得沿链接对 victim 内条目做任何删除/写入
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(victim.join("skills").join("keep"))
            .await
            .unwrap();
        fs::write(victim.join("skills").join("keep").join("SKILL.md"), "keep")
            .await
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, workspace.join(".agents")).unwrap();

        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(Vec::new()),
                subagents: None,
            },
        )
        .await;
        #[cfg(unix)]
        {
            assert!(result.is_err(), "受管根被链接替换时必须拒绝");
            assert_eq!(
                fs::read_to_string(victim.join("skills").join("keep").join("SKILL.md"))
                    .await
                    .unwrap(),
                "keep",
                "链接目标内容必须原样保留"
            );
        }
        #[cfg(not(unix))]
        let _ = result;
    }

    #[tokio::test]
    async fn malicious_skill_names_rejected_at_service_entry_without_mutation() {
        // create-workspace-v2 全链入口级防线：恶意 skillNames 在任何目录
        // 创建/写入前被 400 拒绝
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let session_workspace = user_root.join("ws");
        let result = crate::service::computer_ws::create_workspace_with_agent_store(
            crate::service::computer_ws::CreateAgentStoreParams {
                user_root: &user_root,
                session_workspace: &session_workspace,
                agent_id: "agent-a",
                skill_zip: None,
                skill_urls: Vec::new(),
                skill_url_map: None,
                skill_names: Some(vec!["../../victim".to_string()]),
                update_skill_names: None,
                hook_config: None,
                downloader: None,
                shared_project_id: None,
            },
        )
        .await;
        assert!(result.is_err(), "恶意 skillNames 必须被拒绝");
        assert!(
            !session_workspace.exists(),
            "校验失败不得留下任何已创建目录"
        );
        assert!(!user_root.join(".agent-store").exists(), "store 不得被创建");
    }
}

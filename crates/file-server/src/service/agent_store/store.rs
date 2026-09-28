use super::*;

pub(super) const DYNAMIC_ADD_LOCK: &str = ".dynamic_add.lock";

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

/// V05：受管 ACP 根身份核验——`.agents` + `SYNC_TARGET_DIRS` 各根
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
pub(super) fn canonicalize_names(kind: &str, names: Vec<String>) -> AppResult<Vec<String>> {
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
pub(super) async fn has_dynamic_add_lock(skill_path: &Path) -> bool {
    fs::try_exists(skill_path.join(DYNAMIC_ADD_LOCK))
        .await
        .unwrap_or(false)
}

/// 写入 `.dynamic_add.lock` (标记动态添加的技能)。
pub(super) async fn ensure_dynamic_add_lock(skill_path: &Path) -> AppResult<()> {
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
pub(super) async fn remove_any_if_exists(path: &Path) -> AppResult<()> {
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

    // sync_agents fan-out (.agents → 各家 ACP 目录, 实体复制与 TS legacy 行为一致)
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

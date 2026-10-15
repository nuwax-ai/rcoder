use super::*;

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

pub(super) const MANIFEST_FILE: &str = "manifest.json";
pub(super) const VIEW_LOCK_NAME: &str = ".view.lock";
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
    pub(super) agents: std::collections::BTreeMap<String, SkillViewEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone, PartialEq)]
pub struct SkillViewEntry {
    #[serde(default)]
    pub(super) skills: Vec<String>,
    #[serde(default)]
    pub(super) subagents: Vec<String>,
}

/// 读 manifest；文件不存在 → 空表（新项目）；存在但损坏 → 报错（见模块注释）。
/// 解析成功后校验所有 agent ID 与名字为合法单路径段（F01）——被污染的
/// manifest 名字会在视图校准中被 join 后删除/链接，读回时必须拒绝。
pub(super) async fn read_manifest(project_store_root: &Path) -> AppResult<SkillViewManifest> {
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

/// 项目级 OS 排他锁。锁文件保留原 inode，永不 rename/remove；关闭句柄
/// 即释放，崩溃后由 OS 自动释放。遗留 token/空锁文件可以直接复用，文件
/// 内容及 mtime 不再授予接管权，也不会因旧文件残留等待五分钟。
pub(super) struct ViewGuard {
    _file: std::fs::File,
}

impl ViewGuard {
    pub(super) async fn acquire(project_store_root: &Path) -> AppResult<Self> {
        let lock = project_store_root.join(VIEW_LOCK_NAME);
        let deadline = std::time::Instant::now() + VIEW_LOCK_WAIT;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .await
            .map_err(|error| {
                crate::error::AppError::system(format!(
                    "open agent skill view lock {}: {error}",
                    lock.display()
                ))
            })?
            .into_std()
            .await;
        loop {
            // try_lock 不阻塞 executor；仅正常竞争异步退避，其他错误直接返回。
            match file.try_lock() {
                Ok(()) => return Ok(ViewGuard { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(crate::error::AppError::system(format!(
                        "lock agent skill view {}: {error}",
                        lock.display()
                    )));
                }
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
pub(super) fn normalize_names(names: Vec<String>) -> Vec<String> {
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

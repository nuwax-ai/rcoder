//! skills 推送 + syncAgents (对齐 nuwax `projectService.pushSkillsToWorkspace`
//! + `AgentWorkspaceUtils.syncAgents`)。

use std::path::{Path, PathBuf};

use futures_util::future::try_join_all;
use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::models::SkillFailure;
use crate::service::skill_download::SkillDownloader;
use crate::workspace::{ProjectContext, WorkspaceResolver};

pub struct PushResult {
    pub project_path: String,
    pub updated_skills: Vec<String>,
    pub failed_skills: Vec<SkillFailure>,
}

#[derive(Default)]
pub struct SkillUpdates {
    pub updated_skills: Vec<String>,
    pub failed_skills: Vec<SkillFailure>,
}

/// 推送 skills 到权威源 `.agents/skills`, 再 sync_agents fan-out 到五家 ACP 目录。
/// 来源: 上传 zip (file) 和/或 skillUrls (HTTP fetch zip)。
pub async fn push_skills(
    resolver: &dyn WorkspaceResolver,
    ctx: &ProjectContext,
    zip_path: Option<&Path>,
    skill_urls: Vec<String>,
    downloader: &SkillDownloader,
) -> AppResult<PushResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let updated = push_skills_at(&project_path, zip_path, skill_urls, downloader).await?;
    Ok(PushResult {
        project_path: project_path.to_string_lossy().to_string(),
        updated_skills: updated.updated_skills,
        failed_skills: updated.failed_skills,
    })
}

/// 推送 skills 核心 (path 制): 解压 zip/url → .claude/skills, 再 sync_agents。
/// project 路由 (push_skills) 与 computer 路由共用。
pub async fn push_skills_at(
    workspace: &Path,
    zip_path: Option<&Path>,
    skill_urls: Vec<String>,
    downloader: &SkillDownloader,
) -> AppResult<SkillUpdates> {
    if zip_path.is_none() && skill_urls.is_empty() {
        return Err(AppError::validation(
            "file or skillUrls cannot both be empty",
        ));
    }
    // 权威 agent 目录 = .agents (对齐 nuwax AgentWorkspaceUtils PRIMARY_AGENT_TYPE="agents")
    let primary_skills = workspace.join(".agents").join("skills");
    let primary_agents = workspace.join(".agents").join("agents");
    fs::create_dir_all(&primary_skills).await?;
    fs::create_dir_all(&primary_agents).await?;

    let mut result = SkillUpdates::default();
    if let Some(source) = zip_path {
        import_skill_archive(
            workspace,
            &primary_skills,
            source,
            false,
            &mut result.updated_skills,
        )
        .await?;
    }
    for url in skill_urls {
        let imported = async {
            let downloaded = downloader.download(&url).await?;
            import_skill_archive(
                workspace,
                &primary_skills,
                downloaded.path(),
                true,
                &mut result.updated_skills,
            )
            .await
        }
        .await;
        if let Err(error) = imported {
            tracing::warn!(error = %error, "skill URL failed; remaining skills continue");
            result.failed_skills.push(SkillFailure::from_error(
                None,
                &url,
                "Download/extract skill zip failed",
                &error,
            ));
        }
    }

    sync_agents(workspace).await?;
    let mut seen = std::collections::HashSet::new();
    result
        .updated_skills
        .retain(|name| seen.insert(name.clone()));
    Ok(result)
}

/// push_skills_to_agent_store 参数 (结构化入参, 避免 too_many_arguments)。
pub struct PushToStoreParams<'a> {
    /// agent-store 锚定根 (Local={root}/{userId}, Subvolume=subvolume_base；
    /// 绑定目录场景=配置根)
    pub user_root: &'a Path,
    /// 已解析的会话工作区（F02：显式传入——按 `user_root.join(cid)` 反推在
    /// normalProject/显式绑定目录下会写错目录）
    pub session_workspace: &'a Path,
    pub agent_id: &'a str,
    pub zip_path: Option<&'a Path>,
    pub skill_urls: Vec<String>,
    pub downloader: &'a SkillDownloader,
    /// 共享工作区（normalProject）项目 ID：Some → manifest 并集视图自愈式
    /// 同步（动态技能不进 manifest，仅补链）；None → 目录级软链 + 跨 agent 防线
    pub shared_project_id: Option<&'a str>,
}

/// push-skills to agent-store (对齐 TS pushSkillsToAgentStore)。
/// 动态技能写入实体目录 (打 `.dynamic_add.lock`), 并确保会话工作区软链。
/// 调用前提: 会话工作区 `.agents/skills` 已是软链 (由 create_workspace_with_agent_store 建立)。
pub async fn push_skills_to_agent_store(params: PushToStoreParams<'_>) -> AppResult<SkillUpdates> {
    let PushToStoreParams {
        user_root,
        session_workspace,
        agent_id,
        zip_path,
        skill_urls,
        downloader,
        shared_project_id,
    } = params;
    fs::create_dir_all(session_workspace).await?;

    // 共享工作区防线：现有 store 链指向其他 agent 时 fail-fast（重链会静默
    // 覆盖先驻 agent 技能；manifest 并集视图复刻前的过渡防线）
    crate::service::agent_store::detect_cross_agent_link_conflict(session_workspace, agent_id)
        .await?;

    let (agent_skills_dir, agent_agents_dir) =
        crate::service::agent_store::ensure_agent_store_dirs(user_root, agent_id).await?;

    let mut result = SkillUpdates::default();

    // 处理上传 zip
    if let Some(zip) = zip_path {
        install_skills_from_zip_dynamic(
            zip,
            &agent_skills_dir,
            session_workspace,
            false,
            &mut result.updated_skills,
        )
        .await?;
    }

    // 处理 skill_urls
    for url in &skill_urls {
        let imported = async {
            let downloaded = downloader.download(url).await?;
            install_skills_from_zip_dynamic(
                downloaded.path(),
                &agent_skills_dir,
                session_workspace,
                true,
                &mut result.updated_skills,
            )
            .await
        }
        .await;
        if let Err(error) = imported {
            tracing::warn!(error = %error, "dynamic skill URL failed; remaining skills continue");
            result.failed_skills.push(SkillFailure::from_error(
                None,
                url,
                "Download/extract/install skill zip failed",
                &error,
            ));
        }
    }

    // 共享工作区 → 自愈式视图同步（动态加技能不改变配置清单——不传清单，
    // 仅补新装技能的链）；非共享 → 目录级软链。
    // 链接装配/视图同步与 legacy fan-out、启动 reconciler 共用工作区锁串行，
    // 防止并发互删（store 实体安装在上锁前完成，不碰工作区目录）；
    // 锁序恒为 workspace → ViewGuard。
    let _ws_guard = crate::service::workspace_lock::acquire(session_workspace).await?;
    if let Some(project_id) = shared_project_id {
        crate::service::agent_store::sync_shared_skill_view(
            user_root,
            session_workspace,
            agent_id,
            project_id,
            crate::service::agent_store::SharedSkillLists::default(),
        )
        .await?;
    } else {
        crate::service::agent_store::link_workspace_to_agent_store(
            session_workspace,
            &agent_skills_dir,
            &agent_agents_dir,
        )
        .await?;
    }
    drop(_ws_guard);

    tracing::info!(
        op = "push_skills_to_agent_store",
        updated_skills = result.updated_skills.len(),
        "pushed skills to agent store"
    );
    let mut seen = std::collections::HashSet::new();
    result
        .updated_skills
        .retain(|name| seen.insert(name.clone()));
    Ok(result)
}

/// 解压 zip → 安装所有 skill 子目录到 agent-store (as_dynamic = true)。
/// 动态安装的 skill 打 `.dynamic_add.lock`, prune 时保留。
async fn install_skills_from_zip_dynamic(
    zip_path: &Path,
    agent_skills_dir: &Path,
    session_workspace: &Path,
    from_url: bool,
    updated: &mut Vec<String>,
) -> AppResult<()> {
    let tmp = session_workspace.join(".tmp");
    fs::create_dir_all(&tmp).await?;
    let extract_guard = crate::service::temp_file::tempdir_in(tmp, ".push-skill-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    crate::service::zip::extract_to(zip_path.to_path_buf(), extract_root.clone()).await?;

    // 候选: 优先 skills/ 子目录, 否则顶层非隐藏目录
    let skills_sub = extract_root.join("skills");
    // 必须用 metadata (跟随软链) 而非 file_type/symlink_metadata, 才等价 Path::is_dir()
    let base = if fs::metadata(&skills_sub)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        skills_sub
    } else {
        extract_root
    };
    let mut candidates = 0;
    let mut rd = fs::read_dir(&base).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        candidates += 1;
        crate::service::agent_store::install_skill_dir(
            &entry.path(),
            agent_skills_dir,
            &name,
            true,
        )
        .await?;
        updated.push(name);
    }
    if from_url && candidates == 0 {
        return Err(AppError::validation(
            "No skill directory found in downloaded zip",
        ));
    }
    Ok(())
}

async fn import_skill_archive(
    workspace: &Path,
    primary_skills: &Path,
    source: &Path,
    from_url: bool,
    imported: &mut Vec<String>,
) -> AppResult<()> {
    let parent = workspace.parent().unwrap_or(workspace).to_path_buf();
    let extract_guard = crate::service::temp_file::tempdir_in(parent, ".skills-extract-").await?;
    let extract_root = extract_guard.path().join("content");
    fs::create_dir_all(&extract_root).await?;
    crate::service::zip::extract_to(source.to_path_buf(), extract_root.clone()).await?;
    let skills_dir = match find_skills_dir(&extract_root).await? {
        Some(path) => Some(path),
        None if from_url => Some(extract_root),
        None => None,
    };
    let mut candidates = 0;
    if let Some(skills_dir) = skills_dir {
        let mut entries = fs::read_dir(&skills_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if from_url && (name.starts_with('.') || !entry.file_type().await?.is_dir()) {
                continue;
            }
            candidates += 1;
            let src = entry.path();
            let dst = primary_skills.join(&name);
            remove_dir_if_exists(&dst).await?;
            copy_entry(&src, &dst).await?;
            imported.push(name);
        }
    }
    if from_url && candidates == 0 {
        return Err(AppError::validation(
            "No skill directory found in downloaded zip",
        ));
    }
    Ok(())
}

/// 在 extract_root 下找 skills 目录 (优先根 skills/, 再查一层子目录 skills/)。
async fn find_skills_dir(root: &Path) -> AppResult<Option<PathBuf>> {
    let direct = root.join("skills");
    match fs::metadata(&direct).await {
        Ok(metadata) if metadata.is_dir() => return Ok(Some(direct)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut entries = fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        // 与 TS Dirent.isDirectory 一致：README 等普通文件不可作为父目录扫描。
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let sub = entry.path().join("skills");
        match fs::metadata(&sub).await {
            Ok(metadata) if metadata.is_dir() => return Ok(Some(sub)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// sync_agents fan-out 目标目录: 权威源 .agents/{skills,agents} → 各家 ACP agent 约定目录。
/// 加新 agent 在此追加目录名即可 (注意: .agents 是权威源本身, 不在此列)。
/// 源码实证: claude(.claude) / opencode(.opencode) / codex(.codex) / grok(.grok) / pi(.pi)。
// 临时屏蔽: 暂不接入 grok/pi agent, skills/agents 不再 fan-out 到这两家; 接入时取消注释即恢复。
pub const SYNC_TARGET_DIRS: &[&str] = &[
    ".claude",
    ".opencode",
    ".codex",
    // ".grok",
    // ".pi",
];

/// 所有 agent 目录 (含权威源 .agents + 各家 ACP 目录)。
/// `link_workspace_to_agent_store` 用此列表把每个目录的 {skills,agents} 软链到 agent-store。
// 临时屏蔽: 同 SYNC_TARGET_DIRS, 暂不接入 grok/pi agent, 不为其建目录/软链。
pub const ALL_AGENT_DIRS: &[&str] = &[
    ".agents",
    ".claude",
    ".opencode",
    ".codex",
    // ".grok",
    // ".pi",
];

/// fan-out 版本标识 (SYNC_TARGET_DIRS 派生): sync_agents 写入 `.agents/.sync_version`,
/// 启动 reconciler 据此 O(1) 判断是否需补 sync。加新 agent 改 SYNC_TARGET_DIRS 即自动变版本。
pub fn sync_target_version() -> String {
    SYNC_TARGET_DIRS.join(",")
}

/// 以 `.agents` 为权威源, 全量 fan-out skills/agents 到各家 ACP agent 约定目录
/// (对齐 nuwax AgentWorkspaceUtils syncAgents; PRIMARY_AGENT_TYPE="agents")。
///
/// 2 × SYNC_TARGET_DIRS 个目录 (每个 agent × {skills, agents}) 并发同步
/// (`try_join_all`) 为实体副本，对齐 TS `syncAgents`。agent-store 场景使用
/// [`crate::service::agent_store::link_workspace_to_agent_store`] 单独建立软链。
/// 各 agent 的 hook 配置 (`settings.json` / `hooks.json` / `plugins/` 等) 不受影响。
///
/// 与保留重建 (`computer_ws::preservation`)、启动 reconciler 共用同一把工作区锁
/// (`service::workspace_lock`)，多 Main 副本在共享存储上串行执行，消除并发删/复制竞争。
pub async fn sync_agents(project_path: &Path) -> AppResult<()> {
    match sync_agents_with_wait(project_path, SYNC_AGENTS_WAIT).await? {
        SyncAgentsOutcome::Completed => Ok(()),
        SyncAgentsOutcome::BusySkipped => Err(AppError::system(
            "workspace skill sync is busy, please retry",
        )),
    }
}

/// `sync_agents` 的默认锁等待窗口，对齐共享视图 ViewGuard 的 busy 语义：
/// 等待期满明确报错（可重试），不无限阻塞前台请求。
const SYNC_AGENTS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 带锁等待窗口的 fan-out 结果：`BusySkipped` = 等待窗口内工作区锁被其他
/// 进程/副本占用（其正在同步），本轮跳过、不算错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAgentsOutcome {
    Completed,
    BusySkipped,
}

/// [`sync_agents`] 的可调等待版：启动 reconciler 传短等待，把 busy 记为跳过
/// （另一副本已在同步该工作区）；前台 push 走 [`sync_agents`] 的 30s 长等待。
pub async fn sync_agents_with_wait(
    project_path: &Path,
    wait: std::time::Duration,
) -> AppResult<SyncAgentsOutcome> {
    match crate::service::workspace_lock::acquire_for(project_path, wait).await? {
        None => {
            tracing::info!(
                op = "sync_agents",
                workspace = %project_path.display(),
                "workspace skill sync busy; skipped this pass"
            );
            Ok(SyncAgentsOutcome::BusySkipped)
        }
        Some(_guard) => {
            sync_agents_locked(project_path).await?;
            Ok(SyncAgentsOutcome::Completed)
        }
    }
}

/// [`sync_agents`] 的持锁执行体：调用方必须已持有该工作区的
/// `service::workspace_lock`（进程内 tokio Mutex 不可重入，重复获取会死锁）。
pub(crate) async fn sync_agents_locked(project_path: &Path) -> AppResult<()> {
    let start = std::time::Instant::now();
    let primary_skills = project_path.join(".agents").join("skills");
    let primary_agents = project_path.join(".agents").join("agents");
    let has_skills = crate::service::fs_util::path_exists(&primary_skills).await?;
    let has_agents = crate::service::fs_util::path_exists(&primary_agents).await?;

    // 构建所有 (src, dst, has) 三元组, 创建目标根目录。
    // 目标根被普通文件等占用（真实路径冲突，如用户自建 `.codex` 文件）时跳过
    // 该目标、其余目标继续：不删除用户文件，也不让单个目标永久阻塞整个工作区；
    // 每轮同步重新检查，占用解除后自动恢复。
    let mut copy_targets = Vec::new();
    for agent_root in SYNC_TARGET_DIRS {
        let t_root = project_path.join(agent_root);
        match fs::symlink_metadata(&t_root).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                tracing::info!(
                    op = "sync_agents",
                    target = agent_root,
                    "agent root occupied by non-directory; skip target this pass"
                );
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        fs::create_dir_all(&t_root).await?;
        copy_targets.push((primary_skills.clone(), t_root.join("skills"), has_skills));
        copy_targets.push((primary_agents.clone(), t_root.join("agents"), has_agents));
    }

    // 并发复制: try_join_all 同时 poll 所有 future, 任一失败立即返回。
    // .await 让出点由 tokio 调度。
    try_join_all(
        copy_targets
            .into_iter()
            .map(|(src, dst, has)| async move { sync_dir(&src, &dst, has).await }),
    )
    .await?;

    tracing::info!(
        op = "sync_agents",
        elapsed_ms = start.elapsed().as_millis(),
        targets = SYNC_TARGET_DIRS.len(),
        "skills sync completed"
    );
    // 版本 marker: 仅作同步完成的观测记录——启动 reconciler 已改用名称集比对
    // (见 fanout_names_drifted), 不再读取 marker。字节格式被 file-server-ab
    // diff-rules 钉死 (sha256/size), 不可变更。
    if let Err(e) = fs::write(
        project_path.join(".agents").join(".sync_version"),
        sync_target_version(),
    )
    .await
    {
        tracing::warn!(error = %e, "write sync_version marker failed (best-effort, skipping)");
    }
    Ok(())
}

/// 同步单个目录为实体副本（严格源权威：删后复制，目标为源的精确镜像）。
/// 源不存在时创建空目录。
async fn sync_dir(src: &Path, dst: &Path, has_src: bool) -> AppResult<()> {
    // dst 已是目录链接（agent-store 目录级软链 / 共享视图内链）：链接由
    // store/视图路径管理，legacy fan-out 绝不删除或实体覆盖（`remove_dir_all`
    // 对 symlink 只删链本身，历史实现在此静默毁链）。跳过并留痕，链接校准
    // 仍由受管路径负责。
    if crate::service::agent_store::is_dir_link(dst) {
        tracing::info!(
            op = "sync_dir",
            dst = %dst.display(),
            "fan-out target is a directory link; skip (managed by agent store/view)"
        );
        return Ok(());
    }
    // dst 本身被普通文件等占用 (真实路径冲突, 如用户自建文件): 跳过该目标,
    // 不删用户文件; 每轮复查, 占用解除后自动恢复
    match fs::symlink_metadata(dst).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            tracing::info!(
                op = "sync_dir",
                dst = %dst.display(),
                "fan-out target occupied by non-directory; skip target this pass"
            );
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // 删旧 dst (NotFound 安全)
    match fs::remove_dir_all(dst).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if !has_src {
        fs::create_dir_all(dst).await?;
        return Ok(());
    }
    // TS project/computer legacy fan-out 是实体复制。需要共享 agent-store 的
    // workspace 由独立 link_workspace_to_agent_store 路径负责建立目录链接。
    crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await?;
    Ok(())
}

/// 读取目录顶层条目名称集（目录缺失 = 空集）。
async fn top_level_names(dir: &Path) -> AppResult<std::collections::BTreeSet<String>> {
    let mut names = std::collections::BTreeSet::new();
    let mut rd = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = rd.next_entry().await? {
        names.insert(entry.file_name().to_string_lossy().to_string());
    }
    Ok(names)
}

/// legacy fan-out 是否与权威源发生顶层名称集漂移（启动 reconciler 的自愈判据，
/// 取代只记目标名集合的 `.sync_version` marker 等值比较——marker 无法反映
/// 源后续变化，实体副本漂移后永不自愈）。
///
/// - `.agents/skills` 为目录链接（agent-store 布局）→ 恒不漂移：该布局由
///   store/视图路径管理，legacy fan-out 不得介入（部分退化的链接布局也不补实体）。
/// - 其余：比对源与各**实体目录目标**的顶层条目名称集；目标根或子目录为
///   目录链接、被普通文件占用的目标不参与比较（由受管路径/冲突跳过逻辑
///   负责，与 `sync_agents` 的两级跳过行为一致）。目标整体缺失 →
///   漂移（补一次 fan-out 后自稳）。
pub async fn fanout_names_drifted(project_path: &Path) -> AppResult<bool> {
    let primary_skills = project_path.join(".agents").join("skills");
    if crate::service::agent_store::is_dir_link(&primary_skills) {
        return Ok(false);
    }
    for kind in ["skills", "agents"] {
        let src_names = top_level_names(&project_path.join(".agents").join(kind)).await?;
        for target in SYNC_TARGET_DIRS {
            // 目标根本身是链接或被普通文件占用: sync_agents 对该目标整体跳过
            // (见 sync_agents_locked 的根检查), 这里同样排除比较——否则漂移
            // 判定与同步行为不一致, 该工作区每次重启都空转一次 sync
            match fs::symlink_metadata(project_path.join(target)).await {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    tracing::info!(
                        op = "fanout_names_drifted",
                        target,
                        "fan-out target root is a link or occupied; excluded from drift check"
                    );
                    continue;
                }
                // 根缺失: 正常参与比较 (子路径 NotFound → 判漂移补建)
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let dst = project_path.join(target).join(kind);
            match fs::symlink_metadata(&dst).await {
                Ok(meta) if meta.is_dir() => {
                    if top_level_names(&dst).await? != src_names {
                        return Ok(true);
                    }
                }
                // 目录链接: 由 store/视图路径负责, 不参与比较, 静默
                Ok(meta) if meta.is_symlink() => {}
                // dst 本身被普通文件占用: 真实路径冲突, 留痕供排障
                Ok(_) => {
                    tracing::info!(
                        op = "fanout_names_drifted",
                        target,
                        sub = kind,
                        "fan-out target occupied by non-directory; excluded from drift check"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(false)
}

async fn copy_entry(src: &Path, dst: &Path) -> AppResult<()> {
    let start = std::time::Instant::now();
    let ft = fs::metadata(src).await?;
    let result = if ft.is_dir() {
        crate::service::fs_util::copy_dir_filtered(src, dst, &[], &[]).await
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(src, dst).await?;
        Ok(())
    };
    tracing::debug!(
        op = "copy_entry",
        elapsed_ms = start.elapsed().as_millis(),
        src = %src.display(),
        "file copy completed"
    );
    result
}

async fn remove_dir_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_nested_skills_directory_without_blocking_io() {
        let root_guard = tempfile::tempdir().expect("create test directory");
        let root = root_guard.path().to_path_buf();
        let expected = root.join("bundle").join("skills");
        fs::create_dir_all(&expected)
            .await
            .expect("create skills fixture");

        let found = find_skills_dir(&root).await.expect("scan skills directory");
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn sync_target_version_reflects_target_dirs() {
        // 版本标识 = SYNC_TARGET_DIRS join; 加新 agent 即变版本 (reconciler 据此判断是否需补 sync)
        let v = sync_target_version();
        assert_eq!(v.split(',').count(), SYNC_TARGET_DIRS.len());
        for d in SYNC_TARGET_DIRS {
            assert!(v.contains(*d), "version missing {d}");
        }
    }

    #[tokio::test]
    async fn sync_dir_materializes_source_directory_when_src_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("SKILL.md"), "test").await.unwrap();
        let dst = ws.join(".claude").join("skills");

        sync_dir(&src, &dst, true).await.unwrap();

        assert!(dst.is_dir(), "fan-out target should be a directory");
        assert!(!dst.is_symlink(), "fan-out target should be materialized");
        let content = fs::read_to_string(dst.join("SKILL.md")).await.unwrap();
        assert_eq!(content, "test");
        fs::write(src.join("SKILL.md"), "updated").await.unwrap();
        assert_eq!(
            fs::read_to_string(dst.join("SKILL.md")).await.unwrap(),
            "test",
            "materialized fan-out must not implicitly track source mutations"
        );
    }

    #[tokio::test]
    async fn sync_dir_creates_empty_dir_when_src_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join(".claude").join("skills");
        sync_dir(&tmp.path().join(".agents").join("skills"), &dst, false)
            .await
            .unwrap();
        assert!(dst.is_dir(), "dst should be an empty directory");
    }

    #[tokio::test]
    async fn sync_dir_replaces_existing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("new.md"), "new").await.unwrap();

        let dst = ws.join(".claude").join("skills");
        // 先建旧目录 + 旧文件
        fs::create_dir_all(&dst).await.unwrap();
        fs::write(dst.join("old.md"), "old").await.unwrap();

        sync_dir(&src, &dst, true).await.unwrap();

        // 旧文件已删, 新目录是源目录的实体副本。
        assert!(!dst.join("old.md").exists(), "old file should be gone");
        // 新文件可读
        assert_eq!(fs::read_to_string(dst.join("new.md")).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn sync_dir_skips_directory_link_target_without_unlinking() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("a.md"), "a").await.unwrap();
        // dst 预置为指向独立实体目录的链接 (agent-store/视图受管布局签名)
        let store = ws.join("store").join("skills");
        fs::create_dir_all(&store).await.unwrap();
        fs::write(store.join("b.md"), "b").await.unwrap();
        let dst = ws.join(".claude").join("skills");
        fs::create_dir_all(dst.parent().unwrap()).await.unwrap();
        crate::service::agent_store::force_dir_symlink(&dst, &store)
            .await
            .unwrap();
        assert!(fs::symlink_metadata(&dst).await.unwrap().is_symlink());

        sync_dir(&src, &dst, true).await.unwrap();

        // 链接原样保留 (remove_dir_all 对 symlink 只删链本身——历史行为会毁链),
        // 链接指向的内容不被 fan-out 覆盖, 源条目不落入
        assert!(fs::symlink_metadata(&dst).await.unwrap().is_symlink());
        assert_eq!(fs::read_to_string(dst.join("b.md")).await.unwrap(), "b");
        assert!(!dst.join("a.md").exists());
    }

    #[tokio::test]
    async fn sync_dir_skips_regular_file_target_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let src = ws.join(".agents").join("skills");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("a.md"), "a").await.unwrap();
        // dst 位置被用户普通文件占用 (真实路径冲突) → 跳过且不删文件
        let dst = ws.join(".claude").join("skills");
        fs::create_dir_all(dst.parent().unwrap()).await.unwrap();
        fs::write(&dst, "user data").await.unwrap();

        sync_dir(&src, &dst, true).await.unwrap();

        assert!(fs::symlink_metadata(&dst).await.unwrap().is_file());
        assert_eq!(fs::read_to_string(&dst).await.unwrap(), "user data");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_dir_filtered_fails_on_symlink_cycle_instead_of_recursing() {
        let tmp = tempfile::tempdir().unwrap();
        // 构造环: src/a → loop_dir, loop_dir/b → src —— 物化跟随链接若无
        // 深度上限会无限递归 (栈溢出拖垮进程), 必须明确报错
        let src = tmp.path().join("src");
        let loop_dir = tmp.path().join("loop_dir");
        fs::create_dir_all(&src).await.unwrap();
        fs::create_dir_all(&loop_dir).await.unwrap();
        std::os::unix::fs::symlink("../loop_dir", src.join("a")).unwrap();
        std::os::unix::fs::symlink("../src", loop_dir.join("b")).unwrap();

        let result =
            crate::service::fs_util::copy_dir_filtered(&src, &tmp.path().join("dst"), &[], &[])
                .await;
        let err = result.expect_err("symlink cycle must fail fast");
        assert!(err.to_string().contains("depth"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn sync_agents_skips_conflicted_regular_file_root_and_syncs_others() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let skills = ws.join(".agents").join("skills");
        fs::create_dir_all(skills.join("sk1")).await.unwrap();
        fs::write(skills.join("sk1/SKILL.md"), "x").await.unwrap();
        fs::create_dir_all(ws.join(".agents").join("agents"))
            .await
            .unwrap();
        // `.codex` 被用户普通文件占用 (真实路径冲突, 生产 OpenClaw 现场)
        fs::write(ws.join(".codex"), "user file").await.unwrap();

        sync_agents(ws).await.unwrap();

        // 其余目标正常同步
        assert_eq!(
            fs::read_to_string(ws.join(".claude/skills/sk1/SKILL.md"))
                .await
                .unwrap(),
            "x"
        );
        assert_eq!(
            fs::read_to_string(ws.join(".opencode/skills/sk1/SKILL.md"))
                .await
                .unwrap(),
            "x"
        );
        // 冲突文件不被删除、不改为目录
        let codex_meta = fs::symlink_metadata(ws.join(".codex")).await.unwrap();
        assert!(codex_meta.is_file());
        assert_eq!(
            fs::read_to_string(ws.join(".codex")).await.unwrap(),
            "user file"
        );
        // 可同步目标全部完成 → marker 照常写入 (占用解除后下轮自动补齐 .codex)
        assert_eq!(
            fs::read_to_string(ws.join(".agents/.sync_version"))
                .await
                .unwrap(),
            sync_target_version()
        );
    }

    #[tokio::test]
    async fn sync_agents_with_wait_reports_busy_then_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        fs::create_dir_all(ws.join(".agents/skills/one"))
            .await
            .unwrap();
        fs::write(ws.join(".agents/skills/one/SKILL.md"), "1")
            .await
            .unwrap();

        // 同任务持锁重入: 本地互斥 (tokio Mutex 不可重入) 必须有界返回 busy,
        // 而不是在 lock_owned().await 上无限等待
        let guard = crate::service::workspace_lock::acquire(ws).await.unwrap();
        let busy = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sync_agents_with_wait(ws, std::time::Duration::from_millis(100)),
        )
        .await
        .expect("same-task reentry must be bounded, not hang")
        .unwrap();
        assert_eq!(busy, SyncAgentsOutcome::BusySkipped);
        drop(guard);

        let done = sync_agents_with_wait(ws, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(done, SyncAgentsOutcome::Completed);
        assert!(ws.join(".claude/skills/one/SKILL.md").exists());
    }

    #[tokio::test]
    async fn sync_agents_with_wait_busy_when_other_task_holds_workspace_lock() {
        // 生产形态: 同进程另一任务 (如 push/create) 持工作区锁, reconciler 侧
        // sync_agents_with_wait 短等待必须 busy 跳过而非无限等待本地互斥
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        fs::create_dir_all(ws.join(".agents/skills/one"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let ws_path = ws.to_path_buf();
        let holder = tokio::spawn(async move {
            let _guard = crate::service::workspace_lock::acquire(&ws_path)
                .await
                .unwrap();
            entered_tx.send(()).unwrap();
            let _ = release_rx.await;
        });
        entered_rx.await.unwrap();

        let busy = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sync_agents_with_wait(ws, std::time::Duration::from_millis(150)),
        )
        .await
        .expect("cross-task contention must be bounded, not hang")
        .unwrap();
        assert_eq!(busy, SyncAgentsOutcome::BusySkipped);

        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let done = sync_agents_with_wait(ws, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(done, SyncAgentsOutcome::Completed);
    }

    #[tokio::test]
    async fn fanout_names_drifted_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // 源与全部实体目标名称集一致 → 不漂移
        for dir in [".agents", ".claude", ".opencode", ".codex"] {
            fs::create_dir_all(ws.join(dir).join("skills/sk1"))
                .await
                .unwrap();
            fs::create_dir_all(ws.join(dir).join("agents"))
                .await
                .unwrap();
        }
        assert!(!fanout_names_drifted(ws).await.unwrap());

        // 源新增一个技能、实体副本没有 (生产 15/13 漂移形态) → 漂移
        fs::create_dir_all(ws.join(".agents/skills/sk2"))
            .await
            .unwrap();
        assert!(fanout_names_drifted(ws).await.unwrap());

        // 对齐后恢复不漂移
        for dir in [".claude", ".opencode", ".codex"] {
            fs::create_dir_all(ws.join(dir).join("skills/sk2"))
                .await
                .unwrap();
        }
        assert!(!fanout_names_drifted(ws).await.unwrap());

        // 目标根被普通文件占用 → 该目标不参与比较 (其余一致即不漂移)
        fs::remove_dir_all(ws.join(".codex")).await.unwrap();
        fs::write(ws.join(".codex"), "occupied").await.unwrap();
        assert!(!fanout_names_drifted(ws).await.unwrap());

        // 目标整体缺失 → 漂移 (补一次 fan-out 后自稳)
        let ws2 = tmp.path().join("ws2");
        fs::create_dir_all(ws2.join(".agents/skills/only"))
            .await
            .unwrap();
        assert!(fanout_names_drifted(&ws2).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fanout_names_drifted_false_for_link_managed_source() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("u1");
        let ws = user.join("c1");
        let store = user.join(".agent-store").join("44").join("skills");
        fs::create_dir_all(&store).await.unwrap();
        fs::create_dir_all(ws.join(".agents")).await.unwrap();
        // `.agents/skills` 为目录链 (agent-store 布局): legacy fan-out 不得介入
        std::os::unix::fs::symlink("../../.agent-store/44/skills", ws.join(".agents/skills"))
            .unwrap();
        assert!(!fanout_names_drifted(&ws).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fanout_names_drifted_excludes_symlinked_target_root() {
        // `.codex` 整个被用户链走: sync_agents 对该目标根整体跳过, drift 判据
        // 必须同样排除——否则判定与同步行为不一致, 每次重启都空转一次 sync
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        for dir in [".agents", ".claude", ".opencode"] {
            fs::create_dir_all(ws.join(dir).join("skills/sk1"))
                .await
                .unwrap();
            fs::create_dir_all(ws.join(dir).join("agents"))
                .await
                .unwrap();
        }
        let foreign = tmp.path().join("foreign");
        fs::create_dir_all(&foreign).await.unwrap();
        std::os::unix::fs::symlink("../foreign", ws.join(".codex")).unwrap();

        assert!(!fanout_names_drifted(ws).await.unwrap());

        // 被排除目标不引发误报; 其余实体目标漂移仍可检出
        fs::create_dir_all(ws.join(".agents/skills/sk2"))
            .await
            .unwrap();
        assert!(fanout_names_drifted(ws).await.unwrap());

        // 实体目标对齐后即不漂移 (被链走的 .codex 永远不参与比较)
        for dir in [".claude", ".opencode"] {
            fs::create_dir_all(ws.join(dir).join("skills/sk2"))
                .await
                .unwrap();
        }
        assert!(!fanout_names_drifted(ws).await.unwrap());
    }

    #[tokio::test]
    async fn copy_dir_filtered_materializes_symlinked_source_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        fs::create_dir_all(&src).await.unwrap();
        // 源内目录链接条目 (视图主目录中的技能链形态)
        fs::create_dir_all(store.join("linked-skill"))
            .await
            .unwrap();
        fs::write(store.join("linked-skill/SKILL.md"), "linked")
            .await
            .unwrap();
        crate::service::agent_store::force_dir_symlink(
            &src.join("linked-skill"),
            &store.join("linked-skill"),
        )
        .await
        .unwrap();
        let dst = tmp.path().join("dst");

        crate::service::fs_util::copy_dir_filtered(&src, &dst, &[], &[])
            .await
            .unwrap();

        // 链接条目被物化为实体副本 (历史行为静默跳过 → 副本缺项)
        let copied = dst.join("linked-skill");
        let copied_meta = fs::symlink_metadata(&copied).await.unwrap();
        assert!(copied_meta.is_dir());
        assert!(
            !copied_meta.is_symlink(),
            "linked entry must be materialized"
        );
        assert_eq!(
            fs::read_to_string(copied.join("SKILL.md")).await.unwrap(),
            "linked"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_dir_filtered_materializes_file_link_and_fails_on_broken_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        fs::create_dir_all(&store).await.unwrap();
        fs::write(store.join("note.md"), "file-entry")
            .await
            .unwrap();

        // 源 A: 文件链接条目 → 物化为实体文件
        let src_a = tmp.path().join("src-a");
        fs::create_dir_all(&src_a).await.unwrap();
        std::os::unix::fs::symlink("../store/note.md", src_a.join("note.md")).unwrap();
        crate::service::fs_util::copy_dir_filtered(&src_a, &tmp.path().join("dst-a"), &[], &[])
            .await
            .unwrap();
        let note_meta = fs::symlink_metadata(tmp.path().join("dst-a/note.md"))
            .await
            .unwrap();
        assert!(note_meta.is_file());
        assert!(!note_meta.is_symlink());
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst-a/note.md"))
                .await
                .unwrap(),
            "file-entry"
        );

        // 源 B: 断链 → 明确报错 (Fail Fast), 不静默跳过
        let src_b = tmp.path().join("src-b");
        fs::create_dir_all(&src_b).await.unwrap();
        std::os::unix::fs::symlink("../store/missing.md", src_b.join("broken.md")).unwrap();
        let result =
            crate::service::fs_util::copy_dir_filtered(&src_b, &tmp.path().join("dst-b"), &[], &[])
                .await;
        assert!(result.is_err(), "broken symlink entry must fail fast");
    }

    #[tokio::test]
    async fn sync_agents_concurrent_copy_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // 准备 .agents/skills + .agents/agents
        let skills = ws.join(".agents").join("skills");
        let agents = ws.join(".agents").join("agents");
        fs::create_dir_all(&skills).await.unwrap();
        fs::create_dir_all(&agents).await.unwrap();
        fs::write(skills.join("a.md"), "a").await.unwrap();
        fs::write(agents.join("b.md"), "b").await.unwrap();

        sync_agents(ws).await.unwrap();

        // 验证所有 5 个目标目录都有 skills 和 agents
        for dir in SYNC_TARGET_DIRS {
            let s = ws.join(dir).join("skills").join("a.md");
            let a = ws.join(dir).join("agents").join("b.md");
            assert!(s.exists(), "{dir}/skills/a.md missing");
            assert!(a.exists(), "{dir}/agents/b.md missing");
        }
    }

    #[tokio::test]
    async fn push_to_store_uses_explicit_session_workspace_not_derived() {
        // F02：PushToStoreParams 显式携带已解析 session_workspace——绝不由
        // user_root.join(cid) 反推（normalProject/绑定目录下会写错目录）。
        // 构造 session_workspace 与 user_root.join(cid) **不同**的两个目录，
        // 断言装配/软链作用于前者且后者不被创建。
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("root").join("u1");
        let resolved_ws = tmp.path().join("bound").join("projdir"); // 显式解析结果
        let naive_ws = user_root.join("c-session-1"); // 旧反推路径
        std::fs::create_dir_all(&resolved_ws).unwrap();
        #[allow(clippy::field_reassign_with_default)]
        let downloader = {
            let mut config = crate::config::Config::default();
            config.upload_project_dir = tmp.path().join("dl-parent");
            SkillDownloader::new(&config).expect("downloader")
        };
        push_skills_to_agent_store(PushToStoreParams {
            user_root: &user_root,
            session_workspace: &resolved_ws,
            agent_id: "agent-f02",
            zip_path: None,
            skill_urls: Vec::new(),
            downloader: &downloader,
            shared_project_id: None,
        })
        .await
        .expect("push with explicit workspace");
        // 软链装配发生在**显式**工作区
        assert!(
            resolved_ws.join(".agents").join("skills").exists(),
            "装配必须作用于显式传入的 session_workspace"
        );
        // 旧反推路径不被创建（反推即写错目录的直接证据）
        assert!(
            !naive_ws.exists(),
            "user_root.join(cid) 反推路径不得被创建（F02）"
        );
    }
}

#[cfg(test)]
mod best_effort_tests;

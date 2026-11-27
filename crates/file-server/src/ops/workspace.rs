//! 工作区装配共享实现：init-project-template / push-skills。
//!
//! 壳在 handlers/computer/workspace/*。

use std::path::Path;

use crate::extract::AppJson as Json;
use crate::models::{InitProjectTemplateResult, PushSkillsResult};

use crate::AppState;
use crate::error::AppError;
use crate::service::skills as skills_service;
use crate::service::temp_file::TemporaryFile;
use crate::service::zip;

/// initProjectTemplate 不落盘的模板顶层条目 (对齐 TS `INIT_TEMPLATE_RESERVED_ENTRIES`,
/// nuwax-file-server a782dd2): `.tmp` 是工作区暂存根 (multer 上传与解压 staging
/// 都在其中), 模板自带的顶层 `.tmp` 会覆盖运行时暂存内容——整体跳过。
/// 仅模板初始化启用; 通用 zip 解压 (createProject 等) 行为不变。
const INIT_TEMPLATE_RESERVED_ENTRIES: &[&str] = &[".tmp"];

/// init-project-template 的 workspace 无关核心（返回 workspace 根；
/// 响应拼装归各域壳层）。
pub async fn init_project_template_core(
    state: &AppState,
    ws: std::path::PathBuf,
    data: TemporaryFile,
    enable_git: bool,
) -> Result<std::path::PathBuf, AppError> {
    tokio::fs::create_dir_all(&ws).await?;
    zip::extract_to_preserving_top_level(
        data.path().to_path_buf(),
        ws.clone(),
        INIT_TEMPLATE_RESERVED_ENTRIES,
    )
    .await?;
    // git 双开关: GIT_ENABLED && enableGit → init + initial commit (对齐 nuwax)
    if state.config.git_enabled && enable_git {
        // 模板文件必须进入初始提交。nuwax 先 init 再 commitAllChanges；只调用
        // init_repo 会仅提交 .gitignore，使刚解压的项目文件全部留在未跟踪状态。
        crate::service::git::write::init_and_commit_offloaded(
            ws.clone(),
            "Initial commit".to_string(),
            state.config.git_default_author_name.clone(),
            state.config.git_default_author_email.clone(),
        )
        .await?;
    }
    Ok(ws)
}

/// init-project-template 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn init_project_template_impl(
    state: &AppState,
    ws: std::path::PathBuf,
    data: TemporaryFile,
    enable_git: bool,
) -> Result<Json<InitProjectTemplateResult>, AppError> {
    let ws = init_project_template_core(state, ws, data, enable_git).await?;
    Ok(Json(InitProjectTemplateResult {
        success: true,
        message: "Project template initialized successfully".to_string(),
        workspace_root: ws.display().to_string(),
    }))
}

/// push-skills 结果（updated 为已推送的技能目录名列表）。
pub struct PushedSkills {
    pub updated: Vec<String>,
    pub failed_skills: Vec<crate::models::SkillFailure>,
    pub agent_store_path: Option<String>,
}

/// 成功推送技能时返回与 TypeScript API 一致的消息。
pub fn pushed_skills_message(updated: &[String], in_agent_store: bool) -> String {
    if updated.is_empty() {
        "No valid skill directories found in file or skillUrls".to_string()
    } else if in_agent_store {
        format!(
            "Pushed {} skills to agent store: {}",
            updated.len(),
            updated.join(", ")
        )
    } else {
        format!("Pushed {} skills: {}", updated.len(), updated.join(", "))
    }
}

/// push-skills 参数集 (结构化入参, 避免 too_many_arguments; 同
/// `PushToStoreParams` / `CreateAgentStoreParams` 惯例)。
pub struct PushSkillsParams<'a> {
    /// 会话工作区
    pub ws: &'a Path,
    pub cid: &'a str,
    pub zip_data: Option<&'a TemporaryFile>,
    pub skill_urls: Vec<String>,
    pub agent_id: Option<&'a str>,
    /// 是否允许 agent-store 软链分支 (computer 布局下成立; userapp 开发卷
    /// 布局 parent 是共享卷根, 传 false 一律走 legacy)
    pub allow_agent_store: bool,
    /// agent-store 根锚定 (项目绑定目录场景, 对齐 TS 1.4.5 `getAgentStorePath`
    /// ——store 锚定配置根不随绑定漂移); `None` 保持 `ws.parent()` 派生
    /// (默认布局, 与历史行为逐字节一致)。
    /// 共享工作区（normalProject）项目 ID：Some → 直接 store 模式（不做目录级
    /// 软链探测——共享视图的挂载目录是实体目录，探测必为 false）+ manifest 同步
    pub shared_project_id: Option<&'a str>,
    pub agent_store_root: Option<&'a Path>,
}

/// push-skills 的 workspace 无关核心（扁平签名, **file-server-userapp 跨 crate
/// 调用方依赖**; agent-store 根固定 `ws.parent()` 派生）。
pub async fn push_skills_core(
    state: &AppState,
    ws: &Path,
    cid: &str,
    zip_data: Option<&TemporaryFile>,
    skill_urls: Vec<String>,
    agent_id: Option<&str>,
    allow_agent_store: bool,
) -> Result<PushedSkills, AppError> {
    push_skills_core_with_store(
        state,
        PushSkillsParams {
            ws,
            cid,
            zip_data,
            skill_urls,
            agent_id,
            allow_agent_store,
            agent_store_root: None,
            shared_project_id: None,
        },
    )
    .await
}

/// [`push_skills_core`] 的参数结构变体（computer 侧注入 agent-store 根锚定）。
pub async fn push_skills_core_with_store(
    state: &AppState,
    params: PushSkillsParams<'_>,
) -> Result<PushedSkills, AppError> {
    let PushSkillsParams {
        ws,
        cid,
        zip_data,
        skill_urls,
        agent_id,
        allow_agent_store,
        agent_store_root,
        shared_project_id,
    } = params;
    if !crate::service::fs_util::path_exists(ws).await? {
        return Err(AppError::resource("workspace does not exist"));
    }

    // 有 agentId 且 workspace 已软链 → 写 agent-store; 否则旧路径 (对齐 TS pushSkillsToWorkspace)
    let agent_id = agent_id.map(|s| s.trim()).filter(|s| !s.is_empty());
    let mut agent_store_path = None;
    let updated = if allow_agent_store && let Some(agent_id) = agent_id {
        let skills_path = ws.join(".agents").join("skills");
        // 共享工作区（TS 同款）：挂载目录为实体目录（条目级链接），不做目录级
        // 软链探测，直接走 store 模式 + manifest 视图同步
        if shared_project_id.is_some() || crate::service::agent_store::is_dir_link(&skills_path) {
            let user_root = match agent_store_root {
                Some(root) => root.to_path_buf(),
                None => ws.parent().unwrap_or(ws).to_path_buf(),
            };
            agent_store_path = Some(
                crate::service::agent_store::agent_store_path(&user_root, agent_id)?
                    .to_string_lossy()
                    .into_owned(),
            );
            skills_service::push_skills_to_agent_store(skills_service::PushToStoreParams {
                user_root: &user_root,
                // F02：显式传已解析 ws——底层不再按 user_root.join(cid) 反推
                session_workspace: ws,
                agent_id,
                zip_path: zip_data.map(|file| file.path()),
                skill_urls,
                downloader: &state.skill_downloader,
                shared_project_id,
            })
            .await?
        } else {
            tracing::info!(
                cid,
                agent_id,
                "push skills: agentId present but workspace not symlinked, use legacy path"
            );
            skills_service::push_skills_at(
                ws,
                zip_data.map(|file| file.path()),
                skill_urls,
                &state.skill_downloader,
            )
            .await?
        }
    } else {
        if let Some(agent_id) = agent_id {
            tracing::info!(
                agent_id,
                "push skills: agent-store path disabled, use legacy path"
            );
        }
        skills_service::push_skills_at(
            ws,
            zip_data.map(|file| file.path()),
            skill_urls,
            &state.skill_downloader,
        )
        .await?
    };
    Ok(PushedSkills {
        updated: updated.updated_skills,
        failed_skills: updated.failed_skills,
        agent_store_path,
    })
}

/// push-skills 的 workspace 无关实现（computer 域 TS 响应拼装）。
pub async fn push_skills_impl(
    state: &AppState,
    params: PushSkillsParams<'_>,
    response_workspace_root: &Path,
) -> Result<Json<PushSkillsResult>, AppError> {
    let r = push_skills_core_with_store(state, params).await?;
    let agent_store_path = r.agent_store_path;
    let message = pushed_skills_message(&r.updated, agent_store_path.is_some());
    Ok(Json(PushSkillsResult {
        success: true,
        message,
        workspace_root: response_workspace_root.display().to_string(),
        updated_skills: r.updated,
        failed_skills: r.failed_skills,
        agent_store_path,
    }))
}

#[cfg(test)]
mod template_reserved_tests {
    use super::*;
    use crate::AppState;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn state(root: &Path) -> AppState {
        use crate::Config;
        use crate::service::build_manager::BuildManager;
        use crate::service::dev_server::DevServerManager;
        use crate::service::log_cache::LogCacheManager;
        use crate::service::skill_download::SkillDownloader;
        use crate::workspace::LocalWorkspaceResolver;

        let config = Arc::new(Config {
            project_source_dir: root.join("projects"),
            computer_workspace_dir: root.join("computer"),
            upload_project_dir: root.join("downloads"),
            git_enabled: true,
            ..Config::default()
        });
        AppState {
            resolver: Arc::new(LocalWorkspaceResolver::new(
                config.project_source_dir.clone(),
                config.computer_workspace_dir.clone(),
            )),
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader: Arc::new(SkillDownloader::new(&config).unwrap()),
            config,
            preview: None,
            started_at: std::time::Instant::now(),
        }
    }

    async fn template_zip_as_temp(root: &Path) -> TemporaryFile {
        let file = std::fs::File::create(root.join("tpl-src.zip")).unwrap();
        let mut zip = ::zip::ZipWriter::new(file);
        let options = ::zip::write::SimpleFileOptions::default();
        use std::io::Write as _;
        zip.add_directory(".tmp/", options).unwrap();
        zip.start_file(".tmp/staging.txt", options).unwrap();
        zip.write_all(b"template-owned").unwrap();
        zip.start_file("src/main.py", options).unwrap();
        zip.write_all(b"print('hello')").unwrap();
        zip.finish().unwrap();
        let bytes = std::fs::read(root.join("tpl-src.zip")).unwrap();
        let mut writer = crate::service::temp_file::TemporaryFileWriter::create(
            root,
            "template",
            64 * 1024 * 1024,
        )
        .await
        .unwrap();
        writer.write(&bytes::Bytes::from(bytes)).await.unwrap();
        writer.finish().await.unwrap()
    }

    /// TS 契约 (a782dd2): 模板自带顶层 `.tmp` 整体跳过, 运行时暂存逐字节保留;
    /// 普通文件落地并进入 enableGit 的初始提交。
    #[tokio::test]
    async fn init_template_preserves_runtime_tmp_and_commits_files() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let ws: PathBuf = dir.path().join("computer/u1/c1");
        // 工作区已有运行时暂存
        tokio::fs::create_dir_all(ws.join(".tmp")).await.unwrap();
        tokio::fs::write(ws.join(".tmp/staging.txt"), "runtime-owned")
            .await
            .unwrap();

        let temp = template_zip_as_temp(dir.path()).await;
        init_project_template_core(&state, ws.clone(), temp, true)
            .await
            .unwrap();

        // 暂存逐字节不变; 模板 .tmp 未落入
        assert_eq!(
            tokio::fs::read(ws.join(".tmp/staging.txt")).await.unwrap(),
            b"runtime-owned".to_vec()
        );
        // 普通模板文件落地
        assert_eq!(
            tokio::fs::read(ws.join("src/main.py")).await.unwrap(),
            b"print('hello')".to_vec()
        );
        // enableGit: 初始提交包含普通文件
        let repo = gix::open(&ws).unwrap();
        let tree = repo.head_tree().unwrap();
        assert!(
            tree.lookup_entry_by_path("src/main.py").unwrap().is_some(),
            "template files must enter the initial commit"
        );
        assert!(
            tree.lookup_entry_by_path(".tmp/staging.txt")
                .unwrap()
                .is_none(),
            "skipped template .tmp must not be committed"
        );
    }
}

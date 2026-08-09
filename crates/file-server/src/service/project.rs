//! project 业务逻辑 (create/delete/copy/upload/export/push-skills)。

use std::path::Path;

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::workspace::{ProjectContext, WorkspaceResolver};

// ── delete-project ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedDir {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteResult {
    #[serde(rename = "deletedDirectories")]
    pub deleted: Vec<String>,
    #[serde(rename = "failedDirectories")]
    pub failed: Vec<FailedDir>,
}

/// 删除项目的 4 个关联目录 (upload / project / dist / log)。
/// 个别目录删除失败不中止整批, 收集到 failed (对齐 nuwax deleteProject)。
pub async fn delete_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
) -> AppResult<DeleteResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    // HTTP handler 已在请求带 pid 时先停止旧 dev server；service 只负责目录事务。
    let project_path = resolver.resolve_project(ctx).await?;
    let targets = [
        config.upload_project_dir.join(project_id),
        project_path,
        config.dist_target_dir.join(project_id),
        config.log_base_dir.join(project_id),
    ];
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for target in targets {
        match fs::remove_dir_all(&target).await {
            Ok(()) => deleted.push(target.to_string_lossy().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 目录本就不存在, 不计入失败 (对齐 nuwax force 删除语义)
            }
            Err(e) => failed.push(FailedDir {
                path: target.to_string_lossy().to_string(),
                error: e.to_string(),
            }),
        }
    }
    Ok(DeleteResult { deleted, failed })
}

// ── create-project ─────────────────────────────────────────────────────────────

pub struct CreateResult {
    pub project_path: String,
}

/// 从模板创建项目 (对齐 nuwax createProject): 解压/复制模板 → .npmrc。
/// 依赖统一由启动流程中的 `pnpm install --prefer-offline` 恢复，不复制 node_modules 缓存。
pub async fn create_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    template_type: &str,
) -> AppResult<CreateResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    // templateType 白名单 (对齐 nuwax: 仅允许 react/vue3)
    if !matches!(template_type, "react" | "vue3") {
        return Err(AppError::validation(format!(
            "templateType must be react or vue3, got '{template_type}'"
        )));
    }
    let template_name = match template_type {
        "vue3" => config.init_project_name_vue3.as_str(),
        // react
        _ => config.init_project_name_react.as_str(),
    };
    let project_path = resolver.resolve_project(ctx).await?;
    if try_exists(&project_path).await? {
        return Err(AppError::business("Project directory already exists"));
    }
    fs::create_dir_all(&project_path).await?;

    // 模板: zip 优先, 其次目录
    let template_zip = config.init_project_dir.join(format!("{template_name}.zip"));
    let template_dir = config.init_project_dir.join(template_name);
    let deploy = if try_exists(&template_zip).await? {
        crate::service::zip::extract_to(template_zip, project_path.clone()).await
    } else if try_exists(&template_dir).await? {
        crate::service::fs_util::copy_dir_filtered(
            &template_dir,
            &project_path,
            &config.traverse_exclude_dirs,
            &config.backup_traverse_exclude_files,
        )
        .await
    } else {
        if let Err(e) = fs::remove_dir_all(&project_path).await {
            tracing::warn!(error = %e, "cleanup project_path on missing template failed (skipping)");
        }
        return Err(AppError::system(format!(
            "Template not found: {template_name}"
        )));
    };
    if let Err(e) = deploy {
        if let Err(cleanup_err) = fs::remove_dir_all(&project_path).await {
            tracing::warn!(error = %cleanup_err, "cleanup project_path on deploy failure failed (skipping)");
        }
        return Err(e);
    }

    if let Err(error) = crate::service::fs_util::write_npmrc(&project_path).await {
        if let Err(cleanup_err) = fs::remove_dir_all(&project_path).await {
            tracing::warn!(error = %cleanup_err, "cleanup project_path on npmrc failure failed (skipping)");
        }
        return Err(error);
    }
    // GIT_ENABLED → git init + commit("init project: {id}") (对齐 nuwax createProject)
    if config.git_enabled
        && let Err(e) = crate::service::git::init_and_commit(
            &project_path,
            &format!("init project: {project_id}"),
            &config.git_default_author_name,
            &config.git_default_author_email,
        )
    {
        tracing::warn!(error = %e, "git init/commit after create failed (skipping)");
    }

    Ok(CreateResult {
        project_path: project_path.to_string_lossy().to_string(),
    })
}

// ── copy-project ───────────────────────────────────────────────────────────────

pub struct CopyResult {
    pub source_project_id: String,
    pub target_project_id: String,
    pub target_project_path: String,
}

/// 源项目复制到目标 (对齐 nuwax copyProject): 源/目标各自隔离上下文, 过滤复制 → .npmrc。
pub async fn copy_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    source_ctx: &ProjectContext,
    target_ctx: &ProjectContext,
) -> AppResult<CopyResult> {
    let source_id = source_ctx.project_id.trim();
    let target_id = target_ctx.project_id.trim();
    if source_id.is_empty() || target_id.is_empty() {
        return Err(AppError::validation(
            "sourceProjectId and targetProjectId cannot be empty",
        ));
    }
    let source_path = resolver.resolve_project(source_ctx).await?;
    let target_path = resolver.resolve_project(target_ctx).await?;
    if !try_exists(&source_path).await? {
        return Err(AppError::business("Source project does not exist"));
    }
    if try_exists(&target_path).await? {
        return Err(AppError::business(
            "Target project directory already exists",
        ));
    }
    fs::create_dir_all(&target_path).await?;
    if let Err(e) = crate::service::fs_util::copy_dir_filtered(
        &source_path,
        &target_path,
        &config.traverse_exclude_dirs,
        &config.backup_traverse_exclude_files,
    )
    .await
    {
        if let Err(cleanup_err) = fs::remove_dir_all(&target_path).await {
            tracing::warn!(error = %cleanup_err, "cleanup target_path on copy failure failed (skipping)");
        }
        return Err(e);
    }
    if let Err(error) = crate::service::fs_util::write_npmrc(&target_path).await {
        if let Err(cleanup_err) = fs::remove_dir_all(&target_path).await {
            tracing::warn!(error = %cleanup_err, "cleanup target_path on npmrc failure failed (skipping)");
        }
        return Err(error);
    }
    // GIT_ENABLED → git init + commit("copy project: {src} -> {tgt}") (对齐 nuwax copyProject)
    if config.git_enabled
        && let Err(e) = crate::service::git::init_and_commit(
            &target_path,
            &format!("copy project: {source_id} -> {target_id}"),
            &config.git_default_author_name,
            &config.git_default_author_email,
        )
    {
        tracing::warn!(error = %e, "git init/commit after copy failed (skipping)");
    }

    Ok(CopyResult {
        source_project_id: source_id.to_string(),
        target_project_id: target_id.to_string(),
        target_project_path: target_path.to_string_lossy().to_string(),
    })
}

// ── helpers ────────────────────────────────────────────────────────────────────

async fn try_exists(p: &Path) -> AppResult<bool> {
    fs::try_exists(p)
        .await
        .map_err(|error| AppError::system(format!("check path {}: {error}", p.display())))
}

/// 目录存在且非空。
async fn project_dir_nonempty(p: &Path) -> AppResult<bool> {
    match fs::read_dir(p).await {
        Ok(mut rd) => rd
            .next_entry()
            .await
            .map(|entry| entry.is_some())
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::system(format!(
            "read project directory {}: {error}",
            p.display()
        ))),
    }
}

// ── upload-project (zip 解压覆盖) ───────────────────────────────────────────────

pub struct UploadProjectResult {
    pub project_id: String,
    pub code_version: String,
}

/// 上传项目 zip 部署 (对齐 nuwax uploadProject):
/// 1. 项目目录非空 → 备份当前到 `v{version-1}.zip` (该版本 zip 不存在且非 GIT 模式时),
///    再清空项目目录; (空目录则直接部署, 不备份。)
/// 2. 解压上传 zip 到项目目录 (上传 zip **不**存为版本 zip)。
/// 3. 单顶层目录上提 + 移除 node_modules + 写 .npmrc。
/// 4. GIT_ENABLED → git init + commit(`upload project v{version}`)。
pub async fn upload_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    zip_path: &Path,
) -> AppResult<UploadProjectResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if code_version.trim().is_empty() {
        return Err(AppError::validation("Code version cannot be empty"));
    }
    let version = crate::service::version::parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx).await?;

    let parent = project_path
        .parent()
        .ok_or_else(|| AppError::system("project path has no parent"))?;
    fs::create_dir_all(parent).await?;
    let staging_guard = crate::service::temp_file::tempdir_in(
        parent.to_path_buf(),
        &format!(".upload-{project_id}-staging-"),
    )
    .await?;
    let rollback_guard = crate::service::temp_file::tempdir_in(
        parent.to_path_buf(),
        &format!(".upload-{project_id}-rollback-"),
    )
    .await?;
    let staging = staging_guard.path().join("project");
    let rollback = rollback_guard.path().join("project");
    fs::create_dir_all(&staging).await?;

    // 1. 先完整解压、整理到 staging；失败不触碰现有项目。
    let extract_r = crate::service::zip::extract_to(zip_path.to_path_buf(), staging.clone()).await;
    if let Err(e) = extract_r {
        if let Err(cleanup_err) = fs::remove_dir_all(&staging).await {
            tracing::warn!(error = %cleanup_err, "cleanup staging on extract failure failed (skipping)");
        }
        return Err(e);
    }
    if let Err(error) =
        crate::service::computer_ws::remove_top_level_dir(&staging, &["__MACOSX"]).await
    {
        if let Err(cleanup_err) = fs::remove_dir_all(&staging).await {
            tracing::warn!(error = %cleanup_err, "cleanup staging on tidy failure failed (skipping)");
        }
        return Err(error);
    }
    remove_node_modules(&staging).await;
    if let Err(error) = crate::service::fs_util::write_npmrc(&staging).await {
        if let Err(cleanup_err) = fs::remove_dir_all(&staging).await {
            tracing::warn!(error = %cleanup_err, "cleanup staging on npmrc failure failed (skipping)");
        }
        return Err(error);
    }

    // 2. 非空旧项目按原规则生成版本备份。
    if project_dir_nonempty(&project_path).await? && version >= 1 && !config.git_enabled {
        let prev_zip = crate::service::version::version_zip_path(config, project_id, version - 1);
        if !try_exists(&prev_zip).await? {
            if let Some(parent) = prev_zip.parent() {
                fs::create_dir_all(parent).await?;
            }
            crate::service::zip::pack_dir(
                project_path.clone(),
                prev_zip,
                config.traverse_exclude_dirs.clone(),
                config.backup_traverse_exclude_files.clone(),
            )
            .await?;
        }
    }

    // 3. 同一文件系统内 rename 切换；失败恢复旧项目，避免留下半成品。
    let had_old = try_exists(&project_path).await?;
    if had_old {
        fs::rename(&project_path, &rollback).await?;
    }
    if let Err(e) = fs::rename(&staging, &project_path).await {
        if had_old && let Err(cleanup_err) = fs::rename(&rollback, &project_path).await {
            tracing::warn!(error = %cleanup_err, "rollback rename to restore old project failed (skipping)");
        }
        if let Err(cleanup_err) = fs::remove_dir_all(&staging).await {
            tracing::warn!(error = %cleanup_err, "cleanup staging on rename failure failed (skipping)");
        }
        return Err(e.into());
    }
    if had_old && let Err(cleanup_err) = fs::remove_dir_all(&rollback).await {
        tracing::warn!(error = %cleanup_err, "cleanup rollback backup after upload failed (skipping)");
    }

    // 4. GIT_ENABLED → init + commit
    if config.git_enabled
        && let Err(e) = crate::service::git::init_and_commit(
            &project_path,
            &format!("upload project v{version}"),
            &config.git_default_author_name,
            &config.git_default_author_email,
        )
    {
        tracing::warn!(error = %e, "git init/commit after upload failed (skipping)");
    }

    Ok(UploadProjectResult {
        project_id: project_id.to_string(),
        code_version: code_version.to_string(),
    })
}

/// 移除项目下的 node_modules (对齐 nuwax removeNodeModules): 存在则删;
/// 符号链接只删链接本身 (保留目标作缓存), 普通目录递归删。不存在则 no-op。
async fn remove_node_modules(project_path: &Path) {
    let nm = project_path.join("node_modules");
    // symlink_metadata 探测真实类型 (不跟随 symlink)
    let Ok(meta) = fs::symlink_metadata(&nm).await else {
        return;
    };
    let r = if meta.file_type().is_symlink() {
        fs::remove_file(&nm).await
    } else if meta.is_dir() {
        fs::remove_dir_all(&nm).await
    } else {
        Ok(())
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "remove node_modules failed");
    }
}

// ── export-project (zip 文件流) ─────────────────────────────────────────────────

/// 导出项目 zip: exportType=LATEST 或 zip 不存在时重打; 可选写 cpage_config.json (打包后删)。
/// 返回 zip 路径, 由 handler 流式响应。
pub async fn export_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    export_type: Option<&str>,
    cpage_config: Option<&serde_json::Value>,
) -> AppResult<std::path::PathBuf> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let version = crate::service::version::parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx).await?;
    if !try_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let zip_path = crate::service::version::version_zip_path(config, project_id, version);
    // 非 LATEST: 用现成历史 zip; 不存在则 404 (不重打, 对齐 nuwax exportProject)。
    // LATEST: 写 cpage_config.json (可选) → 重打 → 删除 cpage_config.json。
    if export_type != Some("LATEST") {
        if !try_exists(&zip_path).await? {
            return Err(AppError::resource(format!(
                "Specified version zip file does not exist: {}",
                zip_path.display()
            )));
        }
        return Ok(zip_path);
    }
    // LATEST 重打: 写 cpage_config.json (打包后删除, 避免污染项目目录)
    let config_path = project_path.join("cpage_config.json");
    let config_written = if let Some(cfg) = cpage_config {
        let bytes = serde_json::to_vec(cfg)
            .map_err(|error| AppError::system(format!("serialize cpage_config.json: {error}")))?;
        fs::write(&config_path, bytes).await.map_err(|error| {
            AppError::system(format!("write {}: {error}", config_path.display()))
        })?;
        Some(config_path.clone())
    } else {
        None
    };
    let repack_result = crate::service::zip::pack_dir(
        project_path.clone(),
        zip_path.clone(),
        config.traverse_exclude_dirs.clone(),
        config.backup_traverse_exclude_files.clone(),
    )
    .await;
    if let Some(p) = &config_written
        && let Err(cleanup_err) = fs::remove_file(p).await
    {
        tracing::warn!(error = %cleanup_err, "cleanup temp config file after export failed (skipping)");
    }
    repack_result?;
    if !try_exists(&zip_path).await? {
        return Err(AppError::system("Exported zip file does not exist"));
    }
    Ok(zip_path)
}

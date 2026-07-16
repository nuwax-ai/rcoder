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
    // TODO(Task 1.4): 若请求带 pid, 先停旧 dev server
    let project_path = resolver.resolve_project(ctx);
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
/// `copyNodeModulesFromCache` / `git init` 留待 Task 1.3/1.4。
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
    let template_name = match template_type {
        "vue3" => config.init_project_name_vue3.as_str(),
        // 默认 react (对齐 nuwax templateType 默认)
        _ => config.init_project_name_react.as_str(),
    };
    let project_path = resolver.resolve_project(ctx);
    if project_dir_nonempty(&project_path).await {
        return Err(AppError::business("Project directory already exists"));
    }
    fs::create_dir_all(&project_path).await?;

    // 模板: zip 优先, 其次目录
    let template_zip = config.init_project_dir.join(format!("{template_name}.zip"));
    let template_dir = config.init_project_dir.join(template_name);
    let deploy = if try_exists(&template_zip).await {
        crate::service::zip::extract_to(template_zip, project_path.clone()).await
    } else if try_exists(&template_dir).await {
        crate::service::fs_util::copy_dir_filtered(
            &template_dir,
            &project_path,
            &config.traverse_exclude_dirs,
            &config.backup_traverse_exclude_files,
        )
        .await
    } else {
        let _ = fs::remove_dir_all(&project_path).await;
        return Err(AppError::system(format!("Template not found: {template_name}")));
    };
    if let Err(e) = deploy {
        let _ = fs::remove_dir_all(&project_path).await;
        return Err(e);
    }

    // TODO(Task 1.4): copyNodeModulesFromCache (模板缓存加速)
    // TODO(Task 1.3): GIT_ENABLED → git init + commit
    // npmrc 写失败不致命
    let _ = crate::service::fs_util::write_npmrc(&project_path).await;

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
    let source_path = resolver.resolve_project(source_ctx);
    let target_path = resolver.resolve_project(target_ctx);
    if !try_exists(&source_path).await {
        return Err(AppError::business("Source project does not exist"));
    }
    if project_dir_nonempty(&target_path).await {
        return Err(AppError::business("Target project directory already exists"));
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
        let _ = fs::remove_dir_all(&target_path).await;
        return Err(e);
    }
    let _ = crate::service::fs_util::write_npmrc(&target_path).await;
    // TODO(Task 1.3): GIT init + commit

    Ok(CopyResult {
        source_project_id: source_id.to_string(),
        target_project_id: target_id.to_string(),
        target_project_path: target_path.to_string_lossy().to_string(),
    })
}

// ── helpers ────────────────────────────────────────────────────────────────────

async fn try_exists(p: &Path) -> bool {
    fs::try_exists(p).await.unwrap_or(false)
}

/// 目录存在且非空。
async fn project_dir_nonempty(p: &Path) -> bool {
    match fs::read_dir(p).await {
        Ok(mut rd) => rd.next_entry().await.ok().flatten().is_some(),
        Err(_) => false,
    }
}

// ── upload-project (zip 解压覆盖) ───────────────────────────────────────────────

pub struct UploadProjectResult {
    pub project_id: String,
    pub code_version: String,
}

/// 上传项目 zip: 存到 version zip + 清空项目目录 + 解压覆盖 (对齐 nuwax uploadProject, 简化)。
pub async fn upload_project(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    zip_data: Vec<u8>,
) -> AppResult<UploadProjectResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if code_version.trim().is_empty() {
        return Err(AppError::validation("Code version cannot be empty"));
    }
    let version = crate::service::version::parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx);

    // 存上传 zip 到版本 zip
    let zip_path = crate::service::version::version_zip_path(config, project_id, version);
    if let Some(parent) = zip_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&zip_path, &zip_data).await?;

    // 清空项目目录 + 解压覆盖
    let _ = fs::remove_dir_all(&project_path).await;
    fs::create_dir_all(&project_path).await?;
    crate::service::zip::extract_to(zip_path, project_path.clone()).await?;

    Ok(UploadProjectResult {
        project_id: project_id.to_string(),
        code_version: code_version.to_string(),
    })
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
    let project_path = resolver.resolve_project(ctx);
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    let zip_path = crate::service::version::version_zip_path(config, project_id, version);
    // 可选写 cpage_config.json (打包后删除)
    let config_path = project_path.join("cpage_config.json");
    let config_written = if let Some(cfg) = cpage_config {
        fs::write(&config_path, serde_json::to_vec(cfg).unwrap_or_default())
            .await
            .ok()
            .map(|_| config_path.clone())
    } else {
        None
    };
    let need_repack = export_type == Some("LATEST")
        || !fs::try_exists(&zip_path).await.unwrap_or(false);
    let repack_result = if need_repack {
        crate::service::zip::pack_dir(
            project_path.clone(),
            zip_path.clone(),
            config.traverse_exclude_dirs.clone(),
            config.backup_traverse_exclude_files.clone(),
        )
        .await
    } else {
        Ok(())
    };
    if let Some(p) = &config_written {
        let _ = fs::remove_file(p).await;
    }
    repack_result?;
    if !fs::try_exists(&zip_path).await.unwrap_or(false) {
        return Err(AppError::system("Exported zip file does not exist"));
    }
    Ok(zip_path)
}

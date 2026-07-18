//! 文件上传 (对齐 nuwax `codeService.uploadSingleFile`/`uploadBatchFiles`
//! + `uploadAttachmentFile`)。

use std::path::Path;

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::path_safety::ensure_within;
use crate::service::version;
use crate::workspace::{ProjectContext, WorkspaceResolver};

pub struct UploadSingleResult {
    pub project_id: String,
}

/// 单文件上传 (内存 → 写盘)。非 GIT 先备份; 越界路径抛错 (对齐 nuwax `uploadSingleFile`)。
pub async fn upload_single_file(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    file_path: &str,
    data: Vec<u8>,
    code_version: &str,
) -> AppResult<UploadSingleResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if file_path.trim().is_empty() {
        return Err(AppError::validation("filePath cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;
    let target = ensure_within(&project_path, file_path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&target, data).await?;
    Ok(UploadSingleResult {
        project_id: project_id.to_string(),
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BatchFile {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub size: u64,
}

/// 批量上传。一次备份; 逐个写 (越界跳过); 任一写失败从备份回滚 (对齐 nuwax `uploadBatchFiles`)。
pub async fn upload_batch_files(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    files: Vec<(String, Vec<u8>)>,
    code_version: &str,
) -> AppResult<Vec<BatchFile>> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if files.is_empty() {
        return Err(AppError::validation("files cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let backup = version::backup_project(config, project_id, &project_path, code_version).await?;

    let mut written = Vec::new();
    for (file_path, data) in &files {
        match ensure_within(&project_path, file_path) {
            Ok(target) => {
                if let Some(parent) = target.parent()
                    && let Err(e) = fs::create_dir_all(parent).await
                {
                    tracing::warn!(path = %file_path, error = %e, "mkdir failed, skip");
                    continue;
                }
                if let Err(e) = fs::write(&target, data).await {
                    tracing::warn!(path = %file_path, error = %e, "write failed, rolling back");
                    if let Some(zip) = backup {
                        let _ = version::restore_from_zip(
                            &project_path,
                            &zip,
                            &config.traverse_exclude_dirs,
                            &config.backup_traverse_exclude_files,
                        )
                        .await;
                    }
                    return Err(AppError::system(format!(
                        "batch upload failed at {file_path}: {e}"
                    )));
                }
                written.push(BatchFile {
                    file_path: file_path.clone(),
                    size: data.len() as u64,
                });
            }
            Err(_) => {
                // 越界跳过 (对齐 nuwax `uploadBatchFiles`)
                tracing::warn!(path = %file_path, "skip unsafe path in batch upload");
            }
        }
    }
    Ok(written)
}

pub struct AttachmentResult {
    pub file_name: String,
    pub relative_path: String,
}

/// 附件上传: 存到 `{projectDir}/.attachments/{name}`; 同名加 `_{nanos}` 后缀 (对齐 nuwax `uploadAttachmentFile`)。
pub async fn upload_attachment_file(
    resolver: &dyn WorkspaceResolver,
    ctx: &ProjectContext,
    preferred_name: Option<&str>,
    original_name: &str,
    data: Vec<u8>,
) -> AppResult<AttachmentResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let attachments_dir = project_path.join(".attachments");
    fs::create_dir_all(&attachments_dir).await?;
    let requested = preferred_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| original_name.to_string());
    let base = Path::new(&requested)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| AppError::validation("invalid attachment file name"))?
        .to_string();
    let final_name = unique_name(&attachments_dir, &base);
    let target = attachments_dir.join(&final_name);
    fs::write(&target, data).await?;
    Ok(AttachmentResult {
        relative_path: format!(".attachments/{final_name}"),
        file_name: final_name,
    })
}

/// 目录内同名文件加 `_{nanos}` 后缀避免覆盖。
fn unique_name(dir: &Path, base: &str) -> String {
    if !dir.join(base).exists() {
        return base.to_string();
    }
    let path = Path::new(base);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(base);
    let ext = path.extension().and_then(|s| s.to_str());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    match ext {
        Some(e) => format!("{stem}_{nanos}.{e}"),
        None => format!("{stem}_{nanos}"),
    }
}

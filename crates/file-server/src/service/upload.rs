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

/// 单文件上传 (临时文件 → 流式复制)。非 GIT 先备份; 越界路径抛错。
pub async fn upload_single_file(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    file_path: &str,
    source: &Path,
    code_version: &str,
) -> AppResult<UploadSingleResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if file_path.trim().is_empty() {
        return Err(AppError::validation("filePath cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;
    let target = ensure_within(&project_path, file_path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    crate::service::temp_file::copy_file(source, &target).await?;
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
    files: Vec<(String, std::path::PathBuf)>,
    code_version: &str,
) -> AppResult<Vec<BatchFile>> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if files.is_empty() {
        return Err(AppError::validation("files cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let backup = version::backup_project(config, project_id, &project_path, code_version).await?;

    let mut written = Vec::new();
    for (file_path, source) in &files {
        match ensure_within(&project_path, file_path) {
            Ok(target) => {
                if let Some(parent) = target.parent()
                    && let Err(e) = fs::create_dir_all(parent).await
                {
                    tracing::warn!(path = %file_path, error = %e, "mkdir failed, skip");
                    continue;
                }
                let copied = match crate::service::temp_file::copy_file(source, &target).await {
                    Ok(size) => size,
                    Err(e) => {
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
                };
                written.push(BatchFile {
                    file_path: file_path.clone(),
                    size: copied,
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
    source: &Path,
) -> AppResult<AttachmentResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
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
    let final_name = write_unique_file(&attachments_dir, &base, source).await?;
    Ok(AttachmentResult {
        relative_path: format!(".attachments/{final_name}"),
        file_name: final_name,
    })
}

/// 以 `create_new` 原子创建附件，避免并发请求在 `exists` 与 `write` 之间互相覆盖。
async fn write_unique_file(dir: &Path, base: &str, source: &Path) -> AppResult<String> {
    let path = Path::new(base);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(base);
    let ext = path.extension().and_then(|s| s.to_str());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for attempt in 0_u16..=u16::MAX {
        let file_name = if attempt == 0 {
            base.to_string()
        } else {
            match ext {
                Some(extension) => format!("{stem}_{nanos}_{attempt}.{extension}"),
                None => format!("{stem}_{nanos}_{attempt}"),
            }
        };
        let target = dir.join(&file_name);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::system(format!(
                    "create attachment {}: {error}",
                    target.display()
                )));
            }
        };
        let mut input = fs::File::open(source).await?;
        let copy_result = tokio::io::copy(&mut input, &mut file).await;
        if let Err(error) = copy_result {
            drop(file);
            if let Err(cleanup_error) = fs::remove_file(&target).await
                && cleanup_error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %target.display(), %cleanup_error, "remove incomplete attachment failed");
            }
            return Err(AppError::system(format!(
                "write attachment {}: {error}",
                target.display()
            )));
        }
        return Ok(file_name);
    }

    Err(AppError::system(
        "cannot allocate a unique attachment file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unique_attachment_creation_never_overwrites() {
        let temp = tempfile::tempdir().expect("create attachment test directory");
        let first_source = temp.path().join("first-source");
        let second_source = temp.path().join("second-source");
        fs::write(&first_source, b"first")
            .await
            .expect("write source");
        fs::write(&second_source, b"second")
            .await
            .expect("write source");
        let first = write_unique_file(temp.path(), "report.txt", &first_source)
            .await
            .expect("write first attachment");
        let second = write_unique_file(temp.path(), "report.txt", &second_source)
            .await
            .expect("write second attachment");

        assert_eq!(first, "report.txt");
        assert_ne!(first, second);
        assert_eq!(
            fs::read(temp.path().join(first)).await.expect("read first"),
            b"first"
        );
        assert_eq!(
            fs::read(temp.path().join(second))
                .await
                .expect("read second"),
            b"second"
        );
    }
}

//! 文件管理（upload/list/delete + 压缩包解压）

use std::io::Write;

use chrono::Utc;
use tokio::fs;
use tracing::{info, instrument};

use download_utils::{
    ArchiveError, detect_file_type, extract_tar_gz, extract_zip, normalize_extracted_dir,
};

use super::models::*;
use super::utils::*;

impl super::service::AppService {
    /// 上传文件 / 压缩包。
    ///
    /// 自动判断（魔数）：zip/tar.gz 压缩包 → 解压到 `target` 目录；其它 → 单文件存 `target`。
    /// 单文件：`target`=文件路径（如 `code/app.jar`）；压缩包：`target`=解压目录（如 `code/`）。
    /// 安全：复用 download_utils 的 zip slip + 1GiB 大小防护，叠加 app 根 canonicalize 校验。
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        validate_app_id(app_id)?;
        validate_upload_target(target)?; // create_dir_all 前拦截 ../ 与绝对路径（避免副作用泄漏）
        if file_data.is_empty() {
            return Err(AppOperationError::Validation(
                "file data is empty".to_string(),
            ));
        }
        // 所有参数校验通过后，确保 workspace 就绪（K8s: 建 per-app PVC；Docker: no-op）。
        // 放参数校验后：避免非法参数（../、空文件）导致副作用（建孤儿 PVC）。
        // 支持"先 upload 准备文件 → 再 create 启动"工作流（upload 不依赖 create 已执行）。
        self.ensure_app_workspace_ready(app_id, None).await?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        fs::create_dir_all(&app_dir)
            .await
            .map_err(|e| map_io_error("failed to create app dir", e, false))?;
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;

        // 魔数判断压缩包类型（不靠文件名后缀，app.jar.zip 也能识别为 zip）
        let file_type = detect_file_type(&file_data).to_string();
        match file_type.as_str() {
            "zip" | "tar.gz" => {
                self.extract_archive(
                    app_id,
                    file_data,
                    file_type,
                    target,
                    flatten,
                    &canonical_app_dir,
                )
                .await
            }
            _ => {
                // 单文件分支（target=文件路径，app 根相对）
                let file_path = app_dir.join(target);
                // 防穿越：canonicalize 父目录后校验仍在 app 目录内（与 delete_file 对称）
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|e| map_io_error("failed to create parent dir", e, false))?;
                    ensure_within_app_dir(parent, &canonical_app_dir)?;
                }
                fs::write(&file_path, &file_data)
                    .await
                    .map_err(|e| map_io_error("failed to write file", e, true))?;
                Ok(UploadResult {
                    file_path: target.to_string(),
                    file_size: file_data.len() as u64,
                    uploaded_at: Utc::now().to_rfc3339(),
                    extracted_count: None,
                })
            }
        }
    }

    /// 解压压缩包（zip/tar.gz）到 target 目录（app 根相对）。
    pub(super) async fn extract_archive(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        file_type: String,
        target: &str,
        flatten: bool,
        canonical_app_dir: &std::path::Path,
    ) -> AppResult<UploadResult> {
        let app_dir = self.get_container_app_dir(app_id).await?;
        let dest = app_dir.join(target.trim_end_matches('/'));
        fs::create_dir_all(&dest)
            .await
            .map_err(|e| map_io_error("failed to create extraction dir", e, false))?;
        let canonical_dest = ensure_within_app_dir(&dest, canonical_app_dir)?;

        let file_size = file_data.len() as u64;
        let dest_clone = canonical_dest.clone();
        // spawn_blocking：写临时文件 + 解压（同步 IO，不阻塞 tokio；TempPath 闭包结束自动删）
        let count =
            tokio::task::spawn_blocking(move || -> std::result::Result<usize, ArchiveError> {
                let mut tmp = tempfile::NamedTempFile::new()?;
                tmp.write_all(&file_data)?;
                let tmp_path = tmp.into_temp_path();
                match file_type.as_str() {
                    "tar.gz" => extract_tar_gz(&tmp_path, &dest_clone),
                    "zip" => extract_zip(&tmp_path, &dest_clone),
                    _ => Err(ArchiveError::InvalidArchive(format!(
                        "unsupported: {file_type}"
                    ))),
                }
            })
            .await
            .map_err(|e| AppOperationError::Backend(format!("extraction task failed: {e}")))?
            .map_err(map_archive_error)?;

        if flatten {
            normalize_extracted_dir(&canonical_dest).map_err(map_archive_error)?;
        }
        info!(
            "[APP] archive extracted: {} -> {} ({} files, flatten={})",
            app_id, target, count, flatten
        );
        Ok(UploadResult {
            file_path: target.to_string(),
            file_size,
            uploaded_at: Utc::now().to_rfc3339(),
            extracted_count: Some(count),
        })
    }

    /// 列出文件（app 根目录，或其子目录如 "code"/"data"/"logs"）。
    ///
    /// `subpath` 为 None/空 → 列 app 根；否则列 `app_dir/{subpath}`。返回的 `path` 字段是
    /// **app-root-relative**（如 "code/app.jar"），可直接作为 upload 的 target / delete 的 path，
    /// 与这两个接口的约定一致。防穿越：子目录 canonicalize 后必须仍在 app 目录内。
    #[instrument(skip(self))]
    pub async fn list_files(
        &self,
        app_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        if !app_dir.exists() {
            return Ok(vec![]);
        }
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;
        // subpath 归一化：去尾部 '/'，空 → 列 app 根
        let sub = subpath
            .map(|p| p.trim_end_matches('/'))
            .filter(|p| !p.is_empty());
        let target_dir = match sub {
            Some(p) => {
                let full = app_dir.join(p);
                if !full.exists() {
                    return Ok(vec![]);
                }
                ensure_within_app_dir(&full, &canonical_app_dir)?
            }
            None => canonical_app_dir,
        };
        // 返回 app-root-relative 路径（sub 存在时前缀 "sub/"）
        let rel_prefix = sub.map(|p| format!("{p}/")).unwrap_or_default();
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&target_dir)
            .await
            .map_err(|e| map_io_error("failed to read dir", e, false))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| map_io_error("failed to traverse dir", e, false))?
        {
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| map_io_error("failed to read file metadata", e, false))?;
            files.push(FileInfo {
                path: format!("{rel_prefix}{}", entry.file_name().to_string_lossy()),
                size: metadata.len(),
                is_dir: metadata.is_dir(),
                modified_at: metadata
                    .modified()
                    .map(|t| {
                        let datetime: chrono::DateTime<Utc> = t.into();
                        datetime.to_rfc3339()
                    })
                    .unwrap_or_default(),
            });
        }
        Ok(files)
    }

    /// 删除文件
    #[instrument(skip(self))]
    pub async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        // file_path 相对 app 根目录（与 upload_file 的 target 同约定：可指向 code/ data/ logs/）
        let app_dir = self.get_container_app_dir(app_id).await?;
        if !app_dir.exists() {
            return Err(AppOperationError::NotFound(format!(
                "app dir does not exist: {app_id}"
            )));
        }
        let full_path = app_dir.join(file_path);
        // 先 exists 守卫，避免 canonicalize 对不存在路径抛 OS 错误（导致 500 而非 404）
        if !full_path.exists() {
            return Err(AppOperationError::FileNotFound(format!(
                "file does not exist: {file_path}"
            )));
        }
        // 安全检查：canonicalize 后确保路径仍在 app 目录内（防 ../ 穿越到外部）
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;
        let canonical_path = ensure_within_app_dir(&full_path, &canonical_app_dir)?;

        if canonical_path.is_dir() {
            fs::remove_dir_all(&canonical_path)
                .await
                .map_err(|e| map_io_error("failed to remove dir", e, false))?;
        } else {
            fs::remove_file(&canonical_path)
                .await
                .map_err(|e| map_io_error("failed to remove file", e, true))?;
        }
        info!("[APP] file deleted: {}", file_path);
        Ok(())
    }
}

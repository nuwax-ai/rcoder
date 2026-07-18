//! 有界流式临时文件。
//!
//! 上传和远程下载统一逐块写入磁盘，避免把大文件聚合到 `Vec<u8>`。
//! `TempPath` 在成功、错误和请求取消路径都会自动删除文件。

use std::path::{Path, PathBuf};

use bytes::Bytes;
use tempfile::TempPath;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{AppError, AppResult};

pub struct TemporaryFile {
    path: TempPath,
    size: u64,
}

impl TemporaryFile {
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub async fn into_body(self) -> AppResult<axum::body::Body> {
        let file = File::open(self.path())
            .await
            .map_err(|error| AppError::system(format!("open temporary response file: {error}")))?;
        let stream =
            futures_util::stream::try_unfold((file, self), |(mut file, temporary)| async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                buffer.truncate(read);
                Ok(Some((Bytes::from(buffer), (file, temporary))))
            });
        Ok(axum::body::Body::from_stream(stream))
    }
}

/// 将已有文件作为固定大小分块流返回，不接管或删除源文件。
pub async fn file_body(path: &Path) -> AppResult<axum::body::Body> {
    let file = File::open(path).await.map_err(|error| {
        AppError::system(format!("open response file {}: {error}", path.display()))
    })?;
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0_u8; 64 * 1024];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(read);
        Ok(Some((Bytes::from(buffer), file)))
    });
    Ok(axum::body::Body::from_stream(stream))
}

pub struct TemporaryFileWriter {
    file: File,
    path: TempPath,
    size: u64,
    max_bytes: u64,
}

impl TemporaryFileWriter {
    pub async fn create(parent: &Path, prefix: &str, max_bytes: u64) -> AppResult<Self> {
        tokio::fs::create_dir_all(parent).await?;
        let parent = parent.to_path_buf();
        let prefix = prefix.to_string();
        let named = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new().prefix(&prefix).tempfile_in(parent)
        })
        .await
        .map_err(|error| AppError::system(format!("create temp file task: {error}")))?
        .map_err(|error| AppError::system(format!("create temp file: {error}")))?;
        let (file, path) = named.into_parts();
        Ok(Self {
            file: File::from_std(file),
            path,
            size: 0,
            max_bytes,
        })
    }

    pub async fn write(&mut self, chunk: &Bytes) -> AppResult<()> {
        let chunk_len = u64::try_from(chunk.len())
            .map_err(|error| AppError::validation(format!("file chunk is too large: {error}")))?;
        let next_size = self
            .size
            .checked_add(chunk_len)
            .ok_or_else(|| AppError::validation("file size overflow"))?;
        if next_size > self.max_bytes {
            return Err(AppError::validation(format!(
                "File size exceeds limit (max {} bytes)",
                self.max_bytes
            )));
        }
        self.file
            .write_all(chunk)
            .await
            .map_err(|error| AppError::system(format!("write temp file: {error}")))?;
        self.size = next_size;
        Ok(())
    }

    pub async fn finish(mut self) -> AppResult<TemporaryFile> {
        self.file
            .flush()
            .await
            .map_err(|error| AppError::system(format!("flush temp file: {error}")))?;
        drop(self.file);
        Ok(TemporaryFile {
            path: self.path,
            size: self.size,
        })
    }
}

pub async fn copy_file(source: &Path, target: &Path) -> AppResult<u64> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut input = File::open(source).await.map_err(|error| {
        AppError::system(format!("open temporary file {}: {error}", source.display()))
    })?;
    // fs::copy 会把 tempfile 的 0600 权限复制给业务文件；显式创建目标，
    // 让目标按进程 umask 获得普通业务文件权限。
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(target)
        .await
        .map_err(|error| AppError::system(format!("open target {}: {error}", target.display())))?;
    tokio::io::copy(&mut input, &mut output)
        .await
        .map_err(|error| {
            AppError::system(format!(
                "copy temporary file {} to {}: {error}",
                source.display(),
                target.display()
            ))
        })
}

pub async fn tempdir_in(parent: PathBuf, prefix: &str) -> AppResult<tempfile::TempDir> {
    let prefix = prefix.to_string();
    tokio::fs::create_dir_all(&parent).await?;
    tokio::task::spawn_blocking(move || tempfile::Builder::new().prefix(&prefix).tempdir_in(parent))
        .await
        .map_err(|error| AppError::system(format!("create temp directory task: {error}")))?
        .map_err(|error| AppError::system(format!("create temp directory: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writer_enforces_limit_and_cleans_partial_file() {
        let directory = tempfile::tempdir().expect("create test directory");
        let mut writer = TemporaryFileWriter::create(directory.path(), "bounded-", 3)
            .await
            .expect("create writer");
        writer
            .write(&Bytes::from_static(b"ab"))
            .await
            .expect("write within limit");
        assert!(writer.write(&Bytes::from_static(b"cd")).await.is_err());
        drop(writer);
        let entries = std::fs::read_dir(directory.path())
            .expect("read test directory")
            .count();
        assert_eq!(entries, 0);
    }

    #[tokio::test]
    async fn finished_file_is_removed_when_guard_drops() {
        let directory = tempfile::tempdir().expect("create test directory");
        let mut writer = TemporaryFileWriter::create(directory.path(), "finished-", 16)
            .await
            .expect("create writer");
        writer
            .write(&Bytes::from_static(b"content"))
            .await
            .expect("write fixture");
        let temporary = writer.finish().await.expect("finish writer");
        let path = temporary.path().to_path_buf();
        assert_eq!(temporary.size(), 7);
        assert!(path.is_file());
        drop(temporary);
        assert!(!path.exists());
    }
}

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info};
use reqwest;

use crate::types::{V0FileData, V0ParseResult, ProjectFile, V0ParseError, SyncResult};
use crate::utils::calculate_hash;

/// V0 文件同步器
pub struct V0FileSync {
    base_path: PathBuf,
    client: reqwest::Client,
}

impl V0FileSync {
    /// 创建新的 V0FileSync 实例
    pub fn new<P: AsRef<Path>>(base_path: P) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
            client: reqwest::Client::new(),
        }
    }

    /// 同步 V0 数据到文件系统
    pub async fn sync_files(&self, v0_data: &V0FileData) -> Result<Vec<String>> {
        let parse_result = v0_data.parse_source()?;
        let mut synced_files = Vec::new();

        for file_entry in parse_result.files {
            let file_path = self.base_path.join(&file_entry.file_path);

            // Check if file exists and compare hashes
            if file_path.exists() {
                let existing_content = fs::read_to_string(&file_path).await?;
                let existing_hash = calculate_hash(&existing_content);

                if existing_hash == file_entry.hash {
                    debug!("File {} is already up to date", file_entry.file_path.display());
                    continue;
                }
            }

            // Create directory if it doesn't exist
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).await?;
            }

            // Handle URL files (download content)
            let final_content = if let Some(url) = &file_entry.url {
                self.download_file(url).await?
            } else {
                file_entry.content.clone()
            };

            // Write file
            fs::write(&file_path, final_content).await?;
            info!("Synced file: {}", file_entry.file_path.display());
            synced_files.push(file_entry.file_path.display().to_string());
        }

        Ok(synced_files)
    }

    /// 下载文件
    async fn download_file(&self, url: &str) -> Result<String> {
        let response = self.client.get(url)
            .send()
            .await
            .context("Failed to download file")?;

        if !response.status().is_success() {
            return Err(V0ParseError::NetworkError(
                format!("HTTP status: {}", response.status())
            ).into());
        }

        let bytes = response.bytes().await?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    /// 读取项目文件
    pub async fn read_project_files(&self, ignore_hidden: bool) -> Result<Vec<ProjectFile>> {
        let mut files = Vec::new();

        if !self.base_path.exists() {
            return Ok(files);
        }

        for entry in walkdir::WalkDir::new(&self.base_path) {
            let entry = entry?;
            let path = entry.path();

            // 过滤隐藏文件
            if ignore_hidden && path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with('.'))
                .unwrap_or(false) {
                continue;
            }

            // 过滤 Claude Code 记忆文件
            if path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.eq_ignore_ascii_case("CLAUDE.md"))
                .unwrap_or(false) {
                continue;
            }

            if path.is_file() {
                let relative_path = path.strip_prefix(&self.base_path)?;
                let content = fs::read_to_string(path).await?;
                let hash = calculate_hash(&content);

                files.push(ProjectFile {
                    path: relative_path.to_path_buf(),
                    content,
                    hash,
                    size: entry.metadata()?.len(),
                });
            }
        }

        Ok(files)
    }

  
    /// 完全同步 V0ParseResult 到文件系统（包括删除多余文件）
    ///
    /// 这个函数会确保后端文件系统与前端 V0ParseResult 完全一致：
    /// 1. 更新或写入 V0ParseResult 中存在的文件
    /// 2. 删除后端存在但 V0ParseResult 中不存在的文件
    /// 3. 清理空目录
    pub async fn sync_v0_result(&self, v0_result: &V0ParseResult) -> Result<SyncResult> {
        let mut written_files = Vec::new();
        let mut deleted_files = Vec::new();

        // 1. 先读取当前项目的所有文件
        let current_files = self.read_project_files(true).await?;
        let current_file_set: std::collections::HashSet<_> = current_files
            .iter()
            .map(|f| f.path.clone())
            .collect();

        let target_file_set: std::collections::HashSet<_> = v0_result
            .files
            .iter()
            .map(|f| f.file_path.clone())
            .collect();

        // 2. 写入或更新文件
        for file_entry in &v0_result.files {
            let file_path = self.base_path.join(&file_entry.file_path);

            // 检查文件是否已存在且内容相同
            let should_write = if file_path.exists() {
                let existing_content = fs::read_to_string(&file_path).await.unwrap_or_default();
                let existing_hash = calculate_hash(&existing_content);
                existing_hash != file_entry.hash
            } else {
                true
            };

            if should_write {
                // 创建目录（如果不存在）
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent).await
                        .context(format!("Failed to create directory: {:?}", parent))?;
                }

                // 处理 URL 文件
                let final_content = if let Some(url) = &file_entry.url {
                    self.download_file(url).await?
                } else {
                    file_entry.content.clone()
                };

                // 写入文件
                fs::write(&file_path, final_content).await
                    .context(format!("Failed to write file: {:?}", file_path))?;

                info!("Wrote file: {}", file_entry.file_path.display());
                written_files.push(file_entry.file_path.display().to_string());
            }
        }

        // 3. 删除后端存在但前端不存在的文件
        for file_path in current_file_set.difference(&target_file_set) {
            let full_path = self.base_path.join(file_path);

            // 安全检查：确保我们不删除重要文件
            if self.should_delete_file(&full_path, file_path).await? {
                fs::remove_file(&full_path).await
                    .context(format!("Failed to delete file: {:?}", full_path))?;

                info!("Deleted file: {}", file_path.display());
                deleted_files.push(file_path.display().to_string());
            }
        }

        // 4. 清理空目录
        self.cleanup_empty_directories().await?;

        Ok(SyncResult {
            written_files,
            deleted_files,
            success: true,
        })
    }

    /// 判断是否应该删除文件
    async fn should_delete_file(&self, full_path: &Path, relative_path: &Path) -> Result<bool> {
        // 安全检查：不删除隐藏文件和隐藏目录中的文件（即使在前端中被过滤）
        // 隐藏目录如 .claude、.git、.vscode 等是后端特有的，前端不需要
        if relative_path.iter().any(|component|
            component.to_string_lossy().starts_with('.')
        ) {
            return Ok(false);
        }

        // 安全检查：不删除重要的系统文件
        let important_files = [
            ".gitignore",
            "CLAUDE.md"
        ];

        if important_files.iter().any(|name| relative_path.ends_with(name)) {
            return Ok(false);
        }

        // 检查文件是否在 .git 目录中
        if full_path.components().any(|component|
            component.as_os_str() == ".git"
        ) {
            return Ok(false);
        }

        // 安全检查：不删除任何隐藏目录中的文件
        // 隐藏目录是指目录名称以 "." 开头的目录（如 .claude、.git、.vscode 等）
        if full_path.components().any(|component|
            component.as_os_str().to_string_lossy().starts_with('.')
        ) {
            return Ok(false);
        }

        Ok(true)
    }

    /// 清理空目录
    async fn cleanup_empty_directories(&self) -> Result<()> {
        // 从叶子目录开始，向上清理空目录
        let mut dirs_to_check = std::collections::VecDeque::new();

        // 收集所有子目录
        for entry in walkdir::WalkDir::new(&self.base_path)
            .into_iter()
            .filter_entry(|e| e.file_name().to_string_lossy() != ".git")
            .filter_map(Result::ok)
        {
            if entry.path().is_dir() && entry.path() != self.base_path {
                dirs_to_check.push_back(entry.path().to_path_buf());
            }
        }

        // 按深度排序，从深层开始清理
        dirs_to_check.make_contiguous().sort_by(|a, b| {
            b.components().count().cmp(&a.components().count())
        });

        for dir_path in dirs_to_check {
            // 检查目录是否为空
            let mut is_empty = true;
            match fs::read_dir(&dir_path).await {
                Ok(mut entries) => {
                    while let Some(entry) = entries.next_entry().await.unwrap_or(None) {
                        let file_name = entry.file_name();
                        let file_name_str = file_name.to_string_lossy();

                        // 跳过隐藏文件和 .git
                        if !file_name_str.starts_with('.') && file_name_str != ".git" {
                            is_empty = false;
                            break;
                        }
                    }
                }
                Err(_) => is_empty = false,
            }

            if is_empty {
                fs::remove_dir(&dir_path).await
                    .context(format!("Failed to remove empty directory: {:?}", dir_path))?;
                debug!("Removed empty directory: {:?}", dir_path);
            }
        }

        Ok(())
    }
}
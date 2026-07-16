//! Agent 下载管理器
//!
//! 负责将 Agent 下载到统一缓存目录，并复制/解压到安装目录。
//! 使用 `download_utils` crate 提供的下载和解压功能。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use download_utils::archive::{self, ArchiveError};
use download_utils::{DownloadConfig, Downloader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::error::AgentDownloadError;

/// Validate that an identifier (agent_id or version) is safe for path construction.
///
/// Checks (in priority order):
/// 1. Reject empty strings
/// 2. Reject path traversal sequences (`..`) — security-critical, checked before character allowlist
/// 3. Only allow alphanumeric characters, dash, underscore, and dot
fn validate_download_identifier(id: &str, label: &str) -> Result<(), AgentDownloadError> {
    if id.is_empty() {
        return Err(AgentDownloadError::NotFound(format!("{} is empty", label)));
    }
    // Reject path traversal first (security-critical)
    if id.contains("..") {
        return Err(AgentDownloadError::NotFound(format!(
            "{} contains path traversal: {}",
            label, id
        )));
    }
    // Only allow alphanumeric, dash, underscore, dot, and v/V (semver prefix)
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == 'v' || c == 'V')
    {
        return Err(AgentDownloadError::NotFound(format!(
            "{} contains invalid characters: {}",
            label, id
        )));
    }
    Ok(())
}

/// 下载结果
pub struct DownloadResult {
    /// 缓存目录路径
    pub cache_path: PathBuf,
    /// 下载的文件大小（字节）
    pub file_size: u64,
}

/// Agent 下载管理器
pub struct AgentDownloadManager {
    /// 缓存目录（默认 /app/agent-cache/）
    cache_dir: PathBuf,
    /// 下载器
    downloader: Downloader,
    /// 并发下载锁：key 为 "{agent_id}:{version}"
    download_locks: DashMap<String, Arc<Mutex<()>>>,
}

impl AgentDownloadManager {
    /// 创建新的下载管理器
    ///
    /// # Errors
    /// 如果缓存目录无法创建或没有写入权限，返回错误。
    pub fn new(cache_dir: impl Into<PathBuf>) -> Result<Self, AgentDownloadError> {
        let cache_dir = cache_dir.into();
        Self::ensure_cache_dir(&cache_dir)?;
        Ok(Self {
            cache_dir,
            downloader: Downloader::new(DownloadConfig::default()),
            download_locks: DashMap::new(),
        })
    }

    /// 使用自定义配置创建
    ///
    /// # Errors
    /// 如果缓存目录无法创建或没有写入权限，返回错误。
    pub fn with_config(
        cache_dir: impl Into<PathBuf>,
        config: DownloadConfig,
    ) -> Result<Self, AgentDownloadError> {
        let cache_dir = cache_dir.into();
        Self::ensure_cache_dir(&cache_dir)?;
        Ok(Self {
            cache_dir,
            downloader: Downloader::new(config),
            download_locks: DashMap::new(),
        })
    }

    /// 确保缓存目录存在且可写
    fn ensure_cache_dir(cache_dir: &Path) -> Result<(), AgentDownloadError> {
        // 创建目录（如果不存在）
        if !cache_dir.exists() {
            std::fs::create_dir_all(cache_dir)
                .map_err(|e| AgentDownloadError::Download(download_utils::DownloadError::Io(e)))?;
        }

        // 验证目录是否可写（尝试创建并删除临时文件）
        let test_file = cache_dir.join(".write_test");
        std::fs::write(&test_file, "")
            .map_err(|e| AgentDownloadError::Download(download_utils::DownloadError::Io(e)))?;
        std::fs::remove_file(&test_file).ok(); // 忽略删除失败

        Ok(())
    }

    /// 获取缓存目录
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 检查缓存是否存在
    pub fn is_cached(&self, agent_id: &str, version: &str) -> bool {
        self.version_dir(agent_id, version)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// 获取版本缓存目录路径（版本归一化）
    pub fn version_dir(
        &self,
        agent_id: &str,
        version: &str,
    ) -> Result<PathBuf, AgentDownloadError> {
        let normalized = shared_types::version_util::normalize_version(version)
            .map_err(|e| AgentDownloadError::NotFound(e.to_string()))?;
        Ok(self.cache_dir.join(agent_id).join(normalized))
    }

    /// 获取下载锁键（版本归一化）
    fn lock_key(agent_id: &str, version: &str) -> Result<String, AgentDownloadError> {
        let normalized = shared_types::version_util::normalize_version(version)
            .map_err(|e| AgentDownloadError::NotFound(e.to_string()))?;
        Ok(format!("{}:{}", agent_id, normalized))
    }

    /// 获取下载锁
    fn get_download_lock(
        &self,
        agent_id: &str,
        version: &str,
    ) -> Result<Arc<Mutex<()>>, AgentDownloadError> {
        let key = Self::lock_key(agent_id, version)?;
        Ok(self
            .download_locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    /// 下载到缓存（带并发控制）
    ///
    /// 同一版本只会有一个下载任务，其他请求等待。
    /// 返回 DownloadResult 包含缓存路径和文件大小。
    pub async fn download_to_cache(
        &self,
        agent_id: &str,
        version: &str,
        url: &str,
    ) -> Result<DownloadResult, AgentDownloadError> {
        validate_download_identifier(agent_id, "agent_id")?;
        validate_download_identifier(version, "version")?;

        // 获取锁（同一版本只有一个下载任务）
        let lock = self.get_download_lock(agent_id, version)?;
        let _guard = lock.lock().await;

        // 双重检查：可能其他请求已经下载完成
        if self.is_cached(agent_id, version) {
            info!(
                agent_id = %agent_id,
                version = %version,
                "Agent already cached, skipping download"
            );
            let version_dir = self.version_dir(agent_id, version)?;
            // 获取已缓存文件的大小
            let file_size = self.get_cached_file_size(&version_dir).unwrap_or(0);
            return Ok(DownloadResult {
                cache_path: version_dir,
                file_size,
            });
        }

        // 确保缓存目录存在
        tokio::fs::create_dir_all(&self.cache_dir).await?;

        // 执行下载到临时文件
        let version_dir = self.version_dir(agent_id, version)?;
        let temp_file = self
            .cache_dir
            .join(format!(".download-{}-{}", agent_id, version));

        // 下载文件到临时文件（使用 download_utils）
        let cancel_token = CancellationToken::new();
        let file_size = self
            .downloader
            .download_to_file(url, &temp_file, None, &cancel_token)
            .await
            .map_err(AgentDownloadError::Download)?;

        // 创建版本目录并移动文件
        tokio::fs::create_dir_all(&version_dir).await?;
        // 从 URL 获取真实文件名（优先从 Content-Disposition，其次从 URL 路径）
        let raw_filename = download_utils::get_filename_from_url(url)
            .await
            .unwrap_or_else(|_| "package.tar.gz".to_string());
        let dest_path = version_dir.join(&raw_filename);
        tokio::fs::rename(&temp_file, &dest_path).await?;

        info!(
            agent_id = %agent_id,
            version = %version,
            path = %version_dir.display(),
            file_size = file_size,
            "Agent cached successfully"
        );

        Ok(DownloadResult {
            cache_path: version_dir,
            file_size,
        })
    }

    /// 获取缓存目录中第一个文件的大小
    fn get_cached_file_size(&self, version_dir: &Path) -> Option<u64> {
        let entries = std::fs::read_dir(version_dir).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_file() {
                return entry.metadata().ok().map(|m| m.len());
            }
        }
        None
    }

    /// 判断目录是否存在且非空
    async fn is_non_empty_dir(path: &Path) -> bool {
        match tokio::fs::read_dir(path).await {
            Ok(mut it) => it.next_entry().await.map(|e| e.is_some()).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 从缓存复制到目标目录并解压
    ///
    /// 支持 tar.gz 和 zip 格式的自动解压。
    /// 解压后会规范化目录结构（去除单层 wrapper 目录）。
    pub async fn copy_to_target(
        &self,
        agent_id: &str,
        version: &str,
        target_base: &Path,
    ) -> Result<PathBuf, AgentDownloadError> {
        validate_download_identifier(agent_id, "agent_id")?;
        validate_download_identifier(version, "version")?;

        let source = self.version_dir(agent_id, version)?;
        let target = target_base.join(agent_id).join(version);

        info!(
            agent_id = %agent_id,
            version = %version,
            source = %source.display(),
            target = %target.display(),
            "copy_to_target: starting"
        );

        // 检查源目录
        if !source.exists() {
            return Err(AgentDownloadError::NotFound(format!(
                "{}@{} not in cache",
                agent_id, version
            )));
        }

        // 目标已存在且非空 → 已安装，跳过复制
        // （避免重复解压；更重要的是避免删除"正被 agent 进程读取"的 bundle，引发并发竞态）
        if target.exists() && Self::is_non_empty_dir(&target).await {
            info!(
                agent_id = %agent_id,
                version = %version,
                target = %target.display(),
                "copy_to_target: target already installed, skipping"
            );
            return Ok(target);
        }

        // 目标存在但为空 / 不完整 → 删除后重新解压（确保干净复制）
        if target.exists() {
            tokio::fs::remove_dir_all(&target).await?;
        }

        // 确保父目录存在
        tokio::fs::create_dir_all(target.parent().unwrap()).await?;

        // 查找缓存中的归档文件
        let archive_file = self.find_archive_file(&source).await?;

        info!(
            agent_id = %agent_id,
            version = %version,
            archive_file = ?archive_file,
            "copy_to_target: archive file found"
        );

        if let Some(archive_path) = archive_file {
            // 检测文件类型并解压
            let file_type = archive::detect_file_type_from_path(&archive_path).map_err(|e| {
                AgentDownloadError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            })?;
            info!(
                agent_id = %agent_id,
                version = %version,
                archive = %archive_path.display(),
                file_type = %file_type,
                "Extracting agent archive"
            );

            // 创建目标目录
            tokio::fs::create_dir_all(&target).await?;

            // 在阻塞线程中执行解压（避免阻塞 tokio runtime）
            let target_clone = target.clone();
            let archive_clone = archive_path.clone();
            let file_type_str = file_type.to_string();

            tokio::task::spawn_blocking(move || match file_type_str.as_str() {
                "tar.gz" => archive::extract_tar_gz(&archive_clone, &target_clone),
                "zip" => archive::extract_zip(&archive_clone, &target_clone),
                _ => Err(ArchiveError::InvalidArchive(format!(
                    "unsupported archive type: {}",
                    file_type_str
                ))),
            })
            .await
            .map_err(|e| AgentDownloadError::Io(std::io::Error::other(e.to_string())))?
            .map_err(|e| {
                AgentDownloadError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            })?;

            // 规范化目录结构（去除单层 wrapper）
            archive::normalize_extracted_dir(&target)
                .map_err(|e| AgentDownloadError::Io(std::io::Error::other(e.to_string())))?;

            info!(
                agent_id = %agent_id,
                version = %version,
                target = %target.display(),
                file_type = %file_type,
                "Agent extracted successfully"
            );
        } else {
            // 没有归档文件，直接复制（兼容非归档格式）
            info!(
                agent_id = %agent_id,
                version = %version,
                "No archive found, copying files directly"
            );
            self.copy_dir_recursive(&source, &target).await?;
        }

        Ok(target)
    }

    /// 查找缓存目录中的归档文件
    ///
    /// 优先通过 magic bytes 识别（更可靠），然后通过扩展名兜底。
    async fn find_archive_file(
        &self,
        source_dir: &Path,
    ) -> Result<Option<PathBuf>, AgentDownloadError> {
        let mut entries = tokio::fs::read_dir(source_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // 1. 优先通过 magic bytes 识别（更可靠，不受文件名影响）
            if let Ok(file_type) = archive::detect_file_type_from_path(&path)
                && (file_type == "tar.gz" || file_type == "zip")
            {
                return Ok(Some(path));
            }

            // 2. 通过扩展名兜底（处理 magic bytes 无法识别的边界情况）
            let file_name = entry.file_name().to_string_lossy().to_lowercase();
            if file_name.ends_with(".tar.gz")
                || file_name.ends_with(".tgz")
                || file_name.ends_with(".zip")
            {
                return Ok(Some(path));
            }
        }

        Ok(None)
    }

    /// 递归复制目录
    async fn copy_dir_recursive(
        &self,
        source: &Path,
        target: &Path,
    ) -> Result<(), AgentDownloadError> {
        tokio::fs::create_dir_all(target).await?;

        let mut entries = tokio::fs::read_dir(source).await?;

        while let Some(entry) = entries.next_entry().await? {
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());

            if entry.file_type().await?.is_dir() {
                Box::pin(self.copy_dir_recursive(&source_path, &target_path)).await?;
            } else {
                tokio::fs::copy(&source_path, &target_path).await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_dir_path() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let manager = AgentDownloadManager::new(tmp_dir.path()).unwrap();

        assert_eq!(
            manager.version_dir("codex-acp", "1.0.0").unwrap(),
            tmp_dir.path().join("codex-acp").join("1.0.0")
        );
        assert_eq!(
            manager.version_dir("my-agent", "2.0.0-beta").unwrap(),
            tmp_dir.path().join("my-agent").join("2.0.0-beta")
        );
    }

    #[test]
    fn test_lock_key_format() {
        assert_eq!(
            AgentDownloadManager::lock_key("codex-acp", "1.0.0").unwrap(),
            "codex-acp:1.0.0"
        );
    }

    #[test]
    fn test_is_cached_false() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let manager = AgentDownloadManager::new(tmp_dir.path()).unwrap();

        assert!(!manager.is_cached("codex-acp", "1.0.0"));
    }

    #[test]
    fn test_is_cached_true() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let manager = AgentDownloadManager::new(tmp_dir.path()).unwrap();

        // 创建缓存目录
        let version_dir = manager.version_dir("codex-acp", "1.0.0").unwrap();
        std::fs::create_dir_all(&version_dir).unwrap();

        assert!(manager.is_cached("codex-acp", "1.0.0"));
    }

    #[test]
    fn test_ensure_cache_dir_creates_directory() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cache_dir = tmp_dir.path().join("new-cache");

        assert!(!cache_dir.exists());
        AgentDownloadManager::ensure_cache_dir(&cache_dir).unwrap();
        assert!(cache_dir.exists());
    }

    #[test]
    fn test_ensure_cache_dir_existing_directory() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cache_dir = tmp_dir.path().join("existing-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // 应该成功，不报错
        AgentDownloadManager::ensure_cache_dir(&cache_dir).unwrap();
    }

    #[tokio::test]
    async fn test_copy_to_target() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let manager = AgentDownloadManager::new(tmp_dir.path().join("cache")).unwrap();

        // 创建源目录和文件
        let source_dir = manager.version_dir("test-agent", "1.0.0").unwrap();
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("binary"), b"fake binary").unwrap();

        // 复制到目标
        let target_base = tmp_dir.path().join("target");
        let result = manager
            .copy_to_target("test-agent", "1.0.0", &target_base)
            .await
            .unwrap();

        // 验证目标文件存在
        assert!(result.exists());
        assert_eq!(
            std::fs::read_to_string(result.join("binary")).unwrap(),
            "fake binary"
        );
    }

    #[tokio::test]
    async fn test_copy_to_target_not_cached() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let manager = AgentDownloadManager::new(tmp_dir.path().join("cache")).unwrap();

        let target_base = tmp_dir.path().join("target");
        let result = manager
            .copy_to_target("test-agent", "1.0.0", &target_base)
            .await;

        assert!(matches!(result, Err(AgentDownloadError::NotFound(_))));
    }

    #[test]
    fn test_validate_download_identifier_valid() {
        assert!(validate_download_identifier("codex-acp", "agent_id").is_ok());
        assert!(validate_download_identifier("my_agent", "agent_id").is_ok());
        assert!(validate_download_identifier("agent.v2", "agent_id").is_ok());
        assert!(validate_download_identifier("1.0.0-beta", "version").is_ok());
        assert!(validate_download_identifier("2_3_4", "version").is_ok());
    }

    #[test]
    fn test_validate_download_identifier_empty() {
        assert!(validate_download_identifier("", "agent_id").is_err());
        assert!(validate_download_identifier("", "version").is_err());
    }

    #[test]
    fn test_validate_download_identifier_path_traversal() {
        assert!(validate_download_identifier("..", "agent_id").is_err());
        assert!(validate_download_identifier("../etc", "agent_id").is_err());
        assert!(validate_download_identifier("foo..bar", "agent_id").is_err());
    }

    #[test]
    fn test_validate_download_identifier_invalid_chars() {
        assert!(validate_download_identifier("/etc/passwd", "agent_id").is_err());
        assert!(validate_download_identifier("agent\\name", "agent_id").is_err());
        assert!(validate_download_identifier("agent id", "agent_id").is_err());
        assert!(validate_download_identifier("agent;rm", "agent_id").is_err());
    }
}

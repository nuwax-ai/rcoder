//! Agent 下载管理器
//!
//! 负责将 Agent 下载到统一缓存目录，并复制到 agent-runner 的安装目录。
//! 使用 `download_utils` crate 提供的下载功能。

pub mod error;
pub mod registry_update;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use download_utils::{DownloadConfig, Downloader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;

use error::AgentDownloadError;

/// Validate that an identifier (agent_id or version) is safe for path construction.
///
/// Only allows alphanumeric characters, dash, underscore, and dot.
/// Rejects empty strings and path traversal sequences (`..`).
fn validate_download_identifier(id: &str, label: &str) -> Result<(), AgentDownloadError> {
    if id.is_empty() {
        return Err(AgentDownloadError::NotFound(format!("{} is empty", label)));
    }
    // Only allow alphanumeric, dash, underscore, dot
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(AgentDownloadError::NotFound(format!(
            "{} contains invalid characters: {}",
            label, id
        )));
    }
    // Reject path traversal
    if id.contains("..") {
        return Err(AgentDownloadError::NotFound(format!(
            "{} contains path traversal: {}",
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
    pub fn with_config(cache_dir: impl Into<PathBuf>, config: DownloadConfig) -> Result<Self, AgentDownloadError> {
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
            std::fs::create_dir_all(cache_dir).map_err(|e| {
                AgentDownloadError::Download(download_utils::DownloadError::Io(e))
            })?;
        }

        // 验证目录是否可写（尝试创建并删除临时文件）
        let test_file = cache_dir.join(".write_test");
        std::fs::write(&test_file, "").map_err(|e| {
            AgentDownloadError::Download(download_utils::DownloadError::Io(e))
        })?;
        std::fs::remove_file(&test_file).ok(); // 忽略删除失败

        Ok(())
    }

    /// 获取缓存目录
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 检查缓存是否存在
    pub fn is_cached(&self, agent_id: &str, version: &str) -> bool {
        self.version_dir(agent_id, version).exists()
    }

    /// 获取版本缓存目录路径
    pub fn version_dir(&self, agent_id: &str, version: &str) -> PathBuf {
        self.cache_dir.join(agent_id).join(version)
    }

    /// 获取下载锁键
    fn lock_key(agent_id: &str, version: &str) -> String {
        format!("{}:{}", agent_id, version)
    }

    /// 获取下载锁
    fn get_download_lock(&self, agent_id: &str, version: &str) -> Arc<Mutex<()>> {
        let key = Self::lock_key(agent_id, version);
        self.download_locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
        let lock = self.get_download_lock(agent_id, version);
        let _guard = lock.lock().await;

        // 双重检查：可能其他请求已经下载完成
        if self.is_cached(agent_id, version) {
            info!(
                agent_id = %agent_id,
                version = %version,
                "Agent already cached, skipping download"
            );
            let version_dir = self.version_dir(agent_id, version);
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
        let version_dir = self.version_dir(agent_id, version);
        let temp_file = self.cache_dir.join(format!(".download-{}-{}", agent_id, version));

        // 下载文件到临时文件（使用 download_utils）
        let cancel_token = CancellationToken::new();
        let file_size = self.downloader
            .download_to_file(url, &temp_file, None, &cancel_token)
            .await
            .map_err(AgentDownloadError::Download)?;

        // 创建版本目录并移动文件
        tokio::fs::create_dir_all(&version_dir).await?;
        let raw_filename = url.split('/').next_back().unwrap_or("package.tar.gz");
        // Reject path traversal and use safe default
        if raw_filename.is_empty()
            || raw_filename == "."
            || raw_filename == ".."
            || raw_filename.contains(std::path::MAIN_SEPARATOR)
        {
            return Err(AgentDownloadError::NotFound(
                "invalid filename from URL".into(),
            ));
        }
        let dest_path = version_dir.join(raw_filename);
        tokio::fs::rename(&temp_file, &dest_path)
            .await?;

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

    /// 从缓存复制到目标目录
    pub async fn copy_to_target(
        &self,
        agent_id: &str,
        version: &str,
        target_base: &Path,
    ) -> Result<PathBuf, AgentDownloadError> {
        validate_download_identifier(agent_id, "agent_id")?;
        validate_download_identifier(version, "version")?;

        let source = self.version_dir(agent_id, version);
        let target = target_base.join(agent_id).join(version);

        // 检查源目录
        if !source.exists() {
            return Err(AgentDownloadError::NotFound(format!(
                "{}@{} not in cache",
                agent_id, version
            )));
        }

        // 如果目标已存在，先删除（确保干净复制）
        if target.exists() {
            tokio::fs::remove_dir_all(&target).await?;
        }

        // 确保父目录存在
        tokio::fs::create_dir_all(target.parent().unwrap()).await?;

        // 递归复制
        self.copy_dir_recursive(&source, &target).await?;

        info!(
            agent_id = %agent_id,
            version = %version,
            source = %source.display(),
            target = %target.display(),
            "Agent copied from cache to target"
        );

        Ok(target)
    }

    /// 递归复制目录
    async fn copy_dir_recursive(&self, source: &Path, target: &Path) -> Result<(), AgentDownloadError> {
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
            manager.version_dir("codex-acp", "1.0.0"),
            tmp_dir.path().join("codex-acp").join("1.0.0")
        );
        assert_eq!(
            manager.version_dir("my-agent", "2.0.0-beta"),
            tmp_dir.path().join("my-agent").join("2.0.0-beta")
        );
    }

    #[test]
    fn test_lock_key_format() {
        assert_eq!(
            AgentDownloadManager::lock_key("codex-acp", "1.0.0"),
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
        let version_dir = manager.version_dir("codex-acp", "1.0.0");
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
        let source_dir = manager.version_dir("test-agent", "1.0.0");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("binary"), b"fake binary").unwrap();

        // 复制到目标
        let target_base = tmp_dir.path().join("target");
        let result = manager.copy_to_target("test-agent", "1.0.0", &target_base).await.unwrap();

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
        let result = manager.copy_to_target("test-agent", "1.0.0", &target_base).await;

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

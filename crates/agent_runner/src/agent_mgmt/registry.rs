//! Agent 注册表 (P0-1)
//!
//! Readers access immutable registry snapshots without a map mutex.
//! Writes are infrequent and already construct a complete map from the latest
//! locked file. After persistence succeeds, ArcSwap publishes that map atomically.
//! A reader sees one complete generation throughout its query or traversal.
//!
//! ## 持久化策略
//! Mutations commit under an OS file lock before publishing their in-memory state.
//! 序列化格式保持 `Vec<AgentManifest>`，反序列化时按 (agent_id, version) 分组。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use shared_types::InstallType;
use shared_types::version_util;
use tracing::{info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;

type RegistryEntries = HashMap<String, HashMap<String, AgentManifest>>;

/// Agent 注册表(线程安全，支持多版本)
pub struct AgentRegistry {
    /// 外层 key: agent_id, 内层 key: version (空字符串表示无版本)
    inner: Arc<ArcSwap<RegistryEntries>>,
    writer: Arc<tokio::sync::Mutex<()>>,
    path_manager: PathManager,
}

enum RegistryMutation {
    Upsert {
        manifest: Box<AgentManifest>,
        insert_only: bool,
    },
    Remove {
        agent_id: String,
        version: Option<String>,
    },
    Heal,
}

impl AgentRegistry {
    /// 创建新的注册表(从磁盘加载已有数据)
    ///
    /// 加载后自动清理残留条目：如果非 builtin agent 的安装目录已不存在，
    /// 说明卸载过程中进程被 kill 导致注册表未更新，此时自动移除该条目。
    pub async fn load(path_manager: PathManager) -> AgentMgmtResult<Self> {
        let mut map = Self::read_from_disk(&path_manager.registry_path())?;
        let healed = Self::heal_orphaned_entries(&mut map);
        let registry = Self {
            inner: Arc::new(ArcSwap::from_pointee(map)),
            writer: Arc::new(tokio::sync::Mutex::new(())),
            path_manager,
        };
        // 自愈后立即落盘，保持文件与内存一致
        if healed > 0 {
            registry.commit(RegistryMutation::Heal).await?;
        }
        Ok(registry)
    }

    /// 清理注册表中安装目录已不存在的残留条目（启动时自愈）
    ///
    /// 通过检查每个非 builtin 条目的 binary_path 是否存在来判断是否残留。
    /// 返回被移除的条目数量。
    fn heal_orphaned_entries(map: &mut RegistryEntries) -> usize {
        let mut removed_count = 0;
        for versions in map.values_mut() {
            versions.retain(|_vkey, manifest| {
                if manifest.install_type == InstallType::Builtin {
                    return true;
                }
                let binary_path = PathBuf::from(&manifest.binary_path);
                if !binary_path.exists() {
                    warn!(
                        "[agent_mgmt] Healing orphaned registry entry: agent_id={}, version={:?}, binary_path={} (directory missing)",
                        manifest.agent_id,
                        manifest.version,
                        manifest.binary_path
                    );
                    removed_count += 1;
                    return false;
                }
                true
            });
        }
        // 清理空的 agent_id 条目
        map.retain(|_, versions| !versions.is_empty());
        if removed_count > 0 {
            info!(
                "[agent_mgmt] Healed {} orphaned registry entries on startup",
                removed_count
            );
        }
        removed_count
    }

    /// 安装根目录(供卸载安全检查等场景使用)
    pub fn install_dir(&self) -> &std::path::Path {
        self.path_manager.install_dir()
    }

    /// 访问内部 PathManager（用于构造 agent_dir / version_dir 路径）
    pub fn path_manager(&self) -> &PathManager {
        &self.path_manager
    }

    /// 内存中创建空注册表(用于测试)
    pub fn empty(path_manager: PathManager) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            writer: Arc::new(tokio::sync::Mutex::new(())),
            path_manager,
        }
    }

    /// 列出所有已安装 agent(不含 builtin)，每个 agent_id 返回最新版本
    pub fn list(&self) -> Vec<AgentManifest> {
        let guard = self.inner.load();
        guard
            .values()
            .filter_map(|versions| {
                // 获取最新版本（排除 builtin）
                versions
                    .values()
                    .filter(|m| m.install_type != InstallType::Builtin)
                    .max_by(|a, b| {
                        version_util::compare_versions(
                            a.version.as_deref().unwrap_or("0.0.0"),
                            b.version.as_deref().unwrap_or("0.0.0"),
                        )
                        .unwrap_or(Ordering::Equal)
                    })
                    .cloned()
            })
            .collect()
    }

    /// 查询单个 agent（返回最新版本）
    pub fn get(&self, agent_id: &str) -> Option<AgentManifest> {
        let guard = self.inner.load();
        let versions = guard.get(agent_id)?;
        versions
            .values()
            .max_by(|a, b| {
                version_util::compare_versions(
                    a.version.as_deref().unwrap_or("0.0.0"),
                    b.version.as_deref().unwrap_or("0.0.0"),
                )
                .unwrap_or(Ordering::Equal)
            })
            .cloned()
    }

    /// 查询指定版本的 agent
    pub fn get_version(&self, agent_id: &str, version: &str) -> Option<AgentManifest> {
        let guard = self.inner.load();
        let versions = guard.get(agent_id)?;
        let vkey = version_util::normalize_version(version).ok()?;
        versions.get(&vkey).cloned()
    }

    /// 获取 agent 的所有版本
    #[cfg(test)]
    pub fn get_all_versions(&self, agent_id: &str) -> Vec<AgentManifest> {
        let guard = self.inner.load();
        match guard.get(agent_id) {
            Some(versions) => versions.values().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// 是否已安装（任何版本）
    pub fn contains(&self, agent_id: &str) -> bool {
        let guard = self.inner.load();
        guard.get(agent_id).map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// 是否已安装指定版本（测试用公共 API）
    #[allow(dead_code)]
    pub fn contains_version(&self, agent_id: &str, version: &str) -> bool {
        let guard = self.inner.load();
        let vkey = match version_util::normalize_version(version) {
            Ok(v) => v,
            Err(_) => return false,
        };
        guard
            .get(agent_id)
            .map(|versions| versions.contains_key(&vkey))
            .unwrap_or(false)
    }

    /// Insert a new version. Memory becomes visible only after file commit.
    pub async fn insert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        self.commit(RegistryMutation::Upsert {
            manifest: Box::new(manifest),
            insert_only: true,
        })
        .await?;
        Ok(())
    }

    pub async fn upsert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        self.commit(RegistryMutation::Upsert {
            manifest: Box::new(manifest),
            insert_only: false,
        })
        .await?;
        Ok(())
    }

    pub async fn remove(&self, agent_id: &str) -> AgentMgmtResult<Vec<AgentManifest>> {
        self.commit(RegistryMutation::Remove {
            agent_id: agent_id.into(),
            version: None,
        })
        .await
    }

    pub async fn remove_version(
        &self,
        agent_id: &str,
        version: &str,
    ) -> AgentMgmtResult<AgentManifest> {
        let normalized = version_util::normalize_version(version)?;
        let mut removed = self
            .commit(RegistryMutation::Remove {
                agent_id: agent_id.into(),
                version: Some(normalized),
            })
            .await?;
        removed
            .pop()
            .ok_or_else(|| AgentMgmtError::NotFound(format!("{agent_id}@{version}")))
    }

    /// builtin agent 数量
    pub fn builtin_count(&self) -> usize {
        let guard = self.inner.load();
        guard
            .values()
            .flat_map(|versions| versions.values())
            .filter(|m| m.install_type == InstallType::Builtin)
            .count()
    }

    /// 总数（unique agent_id 数量）
    pub fn total(&self) -> usize {
        self.inner.load().len()
    }

    fn registry_path(&self) -> PathBuf {
        self.path_manager.registry_path()
    }

    /// Serialize accepted mutations. Once handed to the blocking worker, cancellation
    /// of the caller cannot interrupt commit or memory publication.
    async fn commit(&self, mutation: RegistryMutation) -> AgentMgmtResult<Vec<AgentManifest>> {
        let writer = self.writer.clone().lock_owned().await;
        let inner = self.inner.clone();
        let path = self.registry_path();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let _writer = writer;
                let parent = path.parent().ok_or_else(|| {
                    AgentMgmtError::Io(std::io::Error::other("registry path has no parent"))
                })?;
                std::fs::create_dir_all(parent)?;
                // Stable sidecar: locking registry.json itself is invalid across rename.
                let lock = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(path.with_extension("json.lock"))?;
                lock.lock()?;
                // Re-read while holding the OS lock, so independent instances merge
                // operations into the latest committed state rather than stale snapshots.
                let mut next = Self::read_from_disk(&path)?;
                let removed = match mutation {
                    RegistryMutation::Upsert {
                        manifest,
                        insert_only,
                    } => {
                        let manifest = *manifest;
                        let key = version_util::normalize_version(
                            manifest.version.as_deref().unwrap_or("0.0.0"),
                        )?;
                        let versions = next.entry(manifest.agent_id.clone()).or_default();
                        if insert_only && versions.contains_key(&key) {
                            return Err(AgentMgmtError::VersionAlreadyInstalled {
                                agent_id: manifest.agent_id.clone(),
                                version: key,
                            });
                        }
                        versions.insert(key, manifest);
                        Vec::new()
                    }
                    RegistryMutation::Remove { agent_id, version } => {
                        if let Some(version) = version {
                            let versions = next
                                .get_mut(&agent_id)
                                .ok_or_else(|| AgentMgmtError::NotFound(agent_id.clone()))?;
                            let removed = versions.remove(&version).ok_or_else(|| {
                                AgentMgmtError::NotFound(format!("{agent_id}@{version}"))
                            })?;
                            if versions.is_empty() {
                                next.remove(&agent_id);
                            }
                            vec![removed]
                        } else {
                            next.remove(&agent_id)
                                .ok_or(AgentMgmtError::NotFound(agent_id))?
                                .into_values()
                                .collect()
                        }
                    }
                    RegistryMutation::Heal => {
                        Self::heal_orphaned_entries(&mut next);
                        Vec::new()
                    }
                };
                let mut snapshot: Vec<_> = next
                    .values()
                    .flat_map(|versions| versions.values())
                    .collect();
                snapshot.sort_by(|a, b| {
                    a.agent_id.cmp(&b.agent_id).then_with(|| {
                        version_util::compare_versions(
                            a.version.as_deref().unwrap_or("0.0.0"),
                            b.version.as_deref().unwrap_or("0.0.0"),
                        )
                        .unwrap_or(Ordering::Equal)
                    })
                });
                let json = serde_json::to_vec_pretty(&snapshot)?;
                let mut temp = tempfile::NamedTempFile::new_in(parent)?;
                temp.write_all(&json)?;
                temp.as_file().sync_all()?;
                temp.persist(&path)
                    .map_err(|error| AgentMgmtError::Io(error.error))?;
                inner.store(Arc::new(next));
                info!(path = %path.display(), "Agent registry committed");
                Ok(removed)
            })();
            if let Err(ref error) = result {
                warn!(path = %path.display(), %error, "Agent registry commit failed");
            }
            result
        })
        .await
        .map_err(|error| {
            AgentMgmtError::Io(std::io::Error::other(format!(
                "registry commit task failed: {error}"
            )))
        })?
    }

    /// 从磁盘读取，按 (agent_id, version) 分组
    fn read_from_disk(path: &std::path::Path) -> AgentMgmtResult<RegistryEntries> {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(error.into()),
        };
        // A corrupt authoritative snapshot is an error, never an empty registry.
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data)?;
        let mut map: RegistryEntries = HashMap::new();
        for manifest in manifests {
            let key =
                version_util::normalize_version(manifest.version.as_deref().unwrap_or("0.0.0"))?;
            map.entry(manifest.agent_id.clone())
                .or_default()
                .insert(key, manifest);
        }
        Ok(map)
    }
}

/// 归一化平台 key: `{os}-{arch}` 格式（代理到 shared_types）
pub use shared_types::version_util::normalize_platform_key;

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::InstallType;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_pm() -> PathManager {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agent-mgmt-test-{}-{}", std::process::id(), n));
        drop(std::fs::remove_dir_all(&dir));
        PathManager::new_with_root(dir)
    }

    /// 创建测试用 manifest，binary_path 指向临时目录内的真实路径
    #[tokio::test]
    async fn independent_registry_writers_merge_latest_disk_state() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
        let b = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
        a.upsert(sample_manifest_with_version_in("a", "1.0.0", dir.path()))
            .await
            .unwrap();
        b.upsert(sample_manifest_with_version_in("b", "1.0.0", dir.path()))
            .await
            .unwrap();
        let disk = AgentRegistry::read_from_disk(&a.registry_path()).unwrap();
        assert!(disk.contains_key("a"));
        assert!(disk.contains_key("b"));
    }

    #[tokio::test]
    async fn failed_commit_does_not_publish_memory() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
        // A directory at the registry path deterministically prevents file persistence.
        std::fs::create_dir(registry.registry_path()).unwrap();
        assert!(registry.upsert(sample_manifest("a")).await.is_err());
        assert_eq!(registry.total(), 0);
    }

    #[tokio::test]
    async fn retained_reader_snapshot_does_not_block_commits_or_change_generation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
        registry.upsert(sample_manifest("a")).await.unwrap();
        // Retain the actual read guard while committing, rather than cloning the map.
        // Publication must not wait for this reader, and must not mutate its view.
        let old = registry.inner.load();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            registry.upsert(sample_manifest("b")),
        )
        .await
        .expect("reader must not block publication")
        .unwrap();
        assert_eq!(old.len(), 1);
        assert!(old.contains_key("a"));
        assert!(!old.contains_key("b"));
        assert_eq!(registry.total(), 2);
        assert!(registry.contains("b"));
        assert_eq!(
            AgentRegistry::read_from_disk(&registry.registry_path())
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn accepted_commit_survives_caller_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(AgentRegistry::empty(PathManager::new_with_root(
            dir.path().into(),
        )));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(registry.registry_path().with_extension("json.lock"))
            .unwrap();
        lock.lock().unwrap();
        // Poll directly through admission; the held OS lock keeps the accepted
        // blocking commit unfinished without a timing-dependent scheduler race.
        let mut caller = Box::pin(registry.upsert(sample_manifest("a")));
        assert!(futures_util::poll!(caller.as_mut()).is_pending());
        drop(caller);
        assert_eq!(registry.total(), 0);
        drop(lock);
        let _committed =
            tokio::time::timeout(std::time::Duration::from_secs(5), registry.writer.lock())
                .await
                .unwrap();
        assert!(registry.contains("a"));
        assert!(
            AgentRegistry::read_from_disk(&registry.registry_path())
                .unwrap()
                .contains_key("a")
        );
    }

    #[tokio::test]
    async fn concurrent_independent_writers_preserve_all_operations() {
        let dir = tempfile::tempdir().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..16 {
            let registry = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
            tasks.spawn(async move {
                registry
                    .upsert(sample_manifest(&format!("agent-{i}")))
                    .await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let path = PathManager::new_with_root(dir.path().into()).registry_path();
        assert_eq!(AgentRegistry::read_from_disk(&path).unwrap().len(), 16);
    }

    #[tokio::test]
    async fn corrupt_authoritative_registry_is_preserved_on_failed_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = AgentRegistry::empty(PathManager::new_with_root(dir.path().into()));
        for contents in ["", "{broken-json"] {
            std::fs::write(registry.registry_path(), contents).unwrap();
            assert!(registry.upsert(sample_manifest("a")).await.is_err());
            assert_eq!(registry.total(), 0);
            assert_eq!(
                std::fs::read_to_string(registry.registry_path()).unwrap(),
                contents
            );
        }
    }

    fn sample_manifest_in(id: &str, install_dir: &std::path::Path) -> AgentManifest {
        let binary_path = install_dir.join(id).join("bin").join(id);
        let mut m = AgentManifest::new(
            id.into(),
            InstallType::Binary,
            "fake-agent".into(),
            vec![],
            binary_path.to_string_lossy().to_string(),
            1024,
            "executable".into(),
        );
        m.installed_at = 12345;
        m
    }

    fn sample_manifest(id: &str) -> AgentManifest {
        // 回退到临时路径（仅用于不需要 load 自愈的测试）
        let mut m = AgentManifest::new(
            id.into(),
            InstallType::Binary,
            "fake-agent".into(),
            vec![],
            format!("/tmp/{id}/bin/fake-agent"),
            1024,
            "executable".into(),
        );
        m.installed_at = 12345;
        m
    }

    fn sample_manifest_with_version(id: &str, version: &str) -> AgentManifest {
        let mut m = sample_manifest(id);
        m.version = Some(version.to_string());
        m
    }

    /// 创建测试用 manifest，binary_path 指向临时目录内的真实路径（带版本）
    fn sample_manifest_with_version_in(
        id: &str,
        version: &str,
        install_dir: &std::path::Path,
    ) -> AgentManifest {
        let mut m = sample_manifest_in(id, install_dir);
        m.version = Some(version.to_string());
        m
    }

    #[tokio::test]
    async fn insert_list_get_remove() {
        let pm = temp_pm();
        let r = AgentRegistry::empty(pm);
        assert_eq!(r.total(), 0);

        r.insert(sample_manifest("codex-acp")).await.unwrap();
        r.insert(sample_manifest("kimi-cli")).await.unwrap();
        assert_eq!(r.total(), 2);

        let got = r.get("codex-acp").expect("should exist");
        assert_eq!(got.agent_id, "codex-acp");

        assert!(r.contains("kimi-cli"));
        assert!(!r.contains("ghost"));

        let removed = r.remove("kimi-cli").await.unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].agent_id, "kimi-cli");
        assert_eq!(r.total(), 1);
    }

    #[tokio::test]
    async fn insert_rejects_duplicate_exact_version() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        let err = r
            .insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AgentMgmtError::VersionAlreadyInstalled { .. }
        ));
    }

    #[tokio::test]
    async fn insert_allows_different_versions() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .await
            .unwrap();
        assert_eq!(r.total(), 1); // 同一个 agent_id
        assert_eq!(r.get_all_versions("codex-acp").len(), 2);
    }

    #[tokio::test]
    async fn get_returns_latest_version() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "1.5.0"))
            .await
            .unwrap();

        let latest = r.get("codex-acp").unwrap();
        assert_eq!(latest.version.as_deref(), Some("2.0.0"));
    }

    #[tokio::test]
    async fn get_version_returns_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .await
            .unwrap();

        let v1 = r.get_version("codex-acp", "1.0.0").unwrap();
        assert_eq!(v1.version.as_deref(), Some("1.0.0"));

        let v2 = r.get_version("codex-acp", "2.0.0").unwrap();
        assert_eq!(v2.version.as_deref(), Some("2.0.0"));

        assert!(r.get_version("codex-acp", "3.0.0").is_none());
    }

    #[tokio::test]
    async fn contains_version_checks_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();

        assert!(r.contains("codex-acp"));
        assert!(r.contains_version("codex-acp", "1.0.0"));
        assert!(!r.contains_version("codex-acp", "2.0.0"));
        assert!(!r.contains("ghost"));
    }

    #[tokio::test]
    async fn remove_version_removes_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .await
            .unwrap();

        let removed = r.remove_version("codex-acp", "1.0.0").await.unwrap();
        assert_eq!(removed.version.as_deref(), Some("1.0.0"));
        assert_eq!(r.total(), 1);
        assert!(r.contains_version("codex-acp", "2.0.0"));
        assert!(!r.contains_version("codex-acp", "1.0.0"));
    }

    #[tokio::test]
    async fn remove_removes_all_versions() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .await
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .await
            .unwrap();

        let removed = r.remove("codex-acp").await.unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(r.total(), 0);
        assert!(!r.contains("codex-acp"));
    }

    #[tokio::test]
    async fn remove_unknown_returns_not_found() {
        let r = AgentRegistry::empty(temp_pm());
        let err = r.remove("ghost").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::NotFound(_)));
    }

    #[tokio::test]
    async fn upsert_overwrites() {
        let r = AgentRegistry::empty(temp_pm());
        r.upsert(sample_manifest_with_version("a", "1.0.0"))
            .await
            .unwrap();
        r.upsert(sample_manifest_with_version("a", "1.0.0"))
            .await
            .unwrap();
        assert_eq!(r.total(), 1);
    }

    #[tokio::test]
    async fn insert_rejects_invalid_manifest() {
        let r = AgentRegistry::empty(temp_pm());
        let mut m = sample_manifest("../bad");
        m.installed_at = 0;
        let err = r.insert(m).await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn load_persists_and_reloads() {
        let pm = temp_pm();
        let install_dir = pm.install_dir().to_path_buf();
        let r1 = AgentRegistry::empty(pm.clone());
        // 创建 binary_path 目录，防止自愈逻辑清理
        for id in &["alpha", "beta"] {
            for _ver in &["1.0.0", "2.0.0"] {
                let path = install_dir.join(id).join("bin").join(id);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"fake").unwrap();
            }
        }
        r1.insert(sample_manifest_with_version_in(
            "alpha",
            "1.0.0",
            &install_dir,
        ))
        .await
        .unwrap();
        r1.insert(sample_manifest_with_version_in(
            "alpha",
            "2.0.0",
            &install_dir,
        ))
        .await
        .unwrap();
        r1.insert(sample_manifest_with_version_in(
            "beta",
            "1.0.0",
            &install_dir,
        ))
        .await
        .unwrap();

        // 重新加载
        let r2 = AgentRegistry::load(pm).await.unwrap();
        assert_eq!(r2.total(), 2);
        assert!(r2.contains("alpha"));
        assert!(r2.contains("beta"));
        assert_eq!(r2.get_all_versions("alpha").len(), 2);
    }

    #[tokio::test]
    async fn list_filters_builtin() {
        let pm = temp_pm();
        let r = AgentRegistry::empty(pm);
        r.insert(sample_manifest_with_version("user-1", "1.0.0"))
            .await
            .unwrap();
        let mut builtin = sample_manifest_with_version("builtin-1", "1.0.0");
        builtin.install_type = InstallType::Builtin;
        r.insert(builtin).await.unwrap();

        let user_only = r.list();
        assert_eq!(user_only.len(), 1);
        assert_eq!(user_only[0].agent_id, "user-1");
    }

    #[tokio::test]
    async fn compare_versions_basic() {
        use std::cmp::Ordering;
        let cv = version_util::compare_versions;
        assert_eq!(cv("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(cv("1.0.0", "1.0.1").unwrap(), Ordering::Less);
        assert_eq!(cv("1.0.1", "1.0.0").unwrap(), Ordering::Greater);
        assert_eq!(cv("1.0.0", "2.0.0").unwrap(), Ordering::Less);
        assert_eq!(cv("1.2.3", "1.2.4").unwrap(), Ordering::Less);
    }

    #[tokio::test]
    async fn compare_versions_with_v_prefix() {
        use std::cmp::Ordering;
        let cv = version_util::compare_versions;
        assert_eq!(cv("v1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(cv("V2.0.0", "1.9.9").unwrap(), Ordering::Greater);
    }

    #[tokio::test]
    async fn compare_versions_returns_err_on_invalid() {
        assert!(version_util::compare_versions("invalid", "0.0.0").is_err());
    }

    #[tokio::test]
    async fn version_key_normalizes() {
        let nk = version_util::normalize_version;
        // v 前缀归一化
        assert_eq!(nk("v1.0.0").unwrap(), "1.0.0");
        assert_eq!(nk("V2.0.0").unwrap(), "2.0.0");
        // trim
        assert_eq!(nk(" 1.0.0 ").unwrap(), "1.0.0");
        // 已归一化的不变
        assert_eq!(nk("1.0.0").unwrap(), "1.0.0");
        // 相同版本不同表示 → 相同 key
        assert_eq!(nk("v1.0.0").unwrap(), nk("1.0.0").unwrap());
        assert_eq!(nk("V1.0.0").unwrap(), nk("1.0.0").unwrap());
        // 非法版本号 → 错误
        assert!(nk("").is_err());
        assert!(nk("abc").is_err());
        assert!(nk("latest").is_err());
    }

    #[tokio::test]
    async fn normalize_platform_key_amd64() {
        assert_eq!(normalize_platform_key("linux", "amd64"), "linux-x86_64");
        assert_eq!(normalize_platform_key("linux", "x86_64"), "linux-x86_64");
    }

    #[tokio::test]
    async fn normalize_platform_key_arm64() {
        assert_eq!(normalize_platform_key("linux", "arm64"), "linux-arm64");
        assert_eq!(normalize_platform_key("linux", "aarch64"), "linux-arm64");
        assert_eq!(normalize_platform_key("darwin", "arm64"), "darwin-arm64");
    }
}

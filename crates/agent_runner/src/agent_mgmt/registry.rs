//! Agent 注册表 (P0-1)
//!
//! 内存中存 `Mutex<HashMap<agent_id, HashMap<version, AgentManifest>>>`,序列化到 `registry.json`。
//! 支持同一 agent 多个版本并存。
//! 用 `parking_lot::Mutex` 而非 DashMap:
//! - 写少读多(list/check 频繁,install/uninstall 偶尔)
//! - 写时还要同步落盘,Mutex 的"写时锁"语义更直接
//!
//! ## 持久化策略
//! 每次 `insert`/`remove` 立即 `save_to_disk`,启动时 `load_from_disk` 恢复。
//! 序列化格式保持 `Vec<AgentManifest>`，反序列化时按 (agent_id, version) 分组。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use shared_types::InstallType;
use shared_types::version_util;
use tracing::{info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;

/// Agent 注册表(线程安全，支持多版本)
pub struct AgentRegistry {
    /// 外层 key: agent_id, 内层 key: version (空字符串表示无版本)
    inner: Mutex<HashMap<String, HashMap<String, AgentManifest>>>,
    path_manager: PathManager,
}

impl AgentRegistry {
    /// 创建新的注册表(从磁盘加载已有数据)
    ///
    /// 加载后自动清理残留条目：如果非 builtin agent 的安装目录已不存在，
    /// 说明卸载过程中进程被 kill 导致注册表未更新，此时自动移除该条目。
    pub fn load(path_manager: PathManager) -> AgentMgmtResult<Self> {
        let mut map = Self::read_from_disk(&path_manager.registry_path())?;
        let healed = Self::heal_orphaned_entries(&mut map);
        let registry = Self {
            inner: Mutex::new(map),
            path_manager,
        };
        // 自愈后立即落盘，保持文件与内存一致
        if healed > 0 {
            registry.save_to_disk()?;
        }
        Ok(registry)
    }

    /// 清理注册表中安装目录已不存在的残留条目（启动时自愈）
    ///
    /// 通过检查每个非 builtin 条目的 binary_path 是否存在来判断是否残留。
    /// 返回被移除的条目数量。
    fn heal_orphaned_entries(map: &mut HashMap<String, HashMap<String, AgentManifest>>) -> usize {
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
            inner: Mutex::new(HashMap::new()),
            path_manager,
        }
    }

    /// 列出所有已安装 agent(不含 builtin)，每个 agent_id 返回最新版本
    pub fn list(&self) -> Vec<AgentManifest> {
        let guard = self.inner.lock();
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
        let guard = self.inner.lock();
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
        let guard = self.inner.lock();
        let versions = guard.get(agent_id)?;
        let vkey = version_util::normalize_version(version).ok()?;
        versions.get(&vkey).cloned()
    }

    /// 获取 agent 的所有版本
    #[cfg(test)]
    pub fn get_all_versions(&self, agent_id: &str) -> Vec<AgentManifest> {
        let guard = self.inner.lock();
        match guard.get(agent_id) {
            Some(versions) => versions.values().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// 是否已安装（任何版本）
    pub fn contains(&self, agent_id: &str) -> bool {
        let guard = self.inner.lock();
        guard.get(agent_id).map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// 是否已安装指定版本（测试用公共 API）
    #[allow(dead_code)]
    pub fn contains_version(&self, agent_id: &str, version: &str) -> bool {
        let guard = self.inner.lock();
        let vkey = match version_util::normalize_version(version) {
            Ok(v) => v,
            Err(_) => return false,
        };
        guard
            .get(agent_id)
            .map(|versions| versions.contains_key(&vkey))
            .unwrap_or(false)
    }

    /// 插入条目(立即落盘)，拒绝重复的精确版本
    #[allow(dead_code)] // used in tests
    pub fn insert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        let mut guard = self.inner.lock();
        let vkey = version_util::normalize_version(manifest.version.as_deref().unwrap_or("0.0.0"))?;
        let versions = guard.entry(manifest.agent_id.clone()).or_default();
        if versions.contains_key(&vkey) {
            return Err(AgentMgmtError::VersionAlreadyInstalled {
                agent_id: manifest.agent_id.clone(),
                version: manifest.version.as_deref().unwrap_or("none").to_string(),
            });
        }
        versions.insert(vkey, manifest);
        drop(guard);
        self.save_to_disk()
    }

    /// 覆盖式更新(用于 reinstall 场景)
    pub fn upsert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        let mut guard = self.inner.lock();
        let vkey = version_util::normalize_version(manifest.version.as_deref().unwrap_or("0.0.0"))?;
        let versions = guard.entry(manifest.agent_id.clone()).or_default();
        versions.insert(vkey, manifest);
        drop(guard);
        self.save_to_disk()
    }

    /// 删除 agent 的所有版本(立即落盘)
    pub fn remove(&self, agent_id: &str) -> AgentMgmtResult<Vec<AgentManifest>> {
        let mut guard = self.inner.lock();
        let removed = guard
            .remove(agent_id)
            .ok_or_else(|| AgentMgmtError::NotFound(agent_id.to_string()))?;
        let removed_vec: Vec<AgentManifest> = removed.into_values().collect();
        drop(guard);
        self.save_to_disk()?;
        Ok(removed_vec)
    }

    /// 删除指定版本(立即落盘)
    pub fn remove_version(&self, agent_id: &str, version: &str) -> AgentMgmtResult<AgentManifest> {
        let mut guard = self.inner.lock();
        let vkey = version_util::normalize_version(version)?;
        let versions = guard
            .get_mut(agent_id)
            .ok_or_else(|| AgentMgmtError::NotFound(agent_id.to_string()))?;
        let removed = versions
            .remove(&vkey)
            .ok_or_else(|| AgentMgmtError::NotFound(format!("{}@{}", agent_id, version)))?;
        // 如果 agent 没有任何版本了，清理空条目
        if versions.is_empty() {
            guard.remove(agent_id);
        }
        drop(guard);
        self.save_to_disk()?;
        Ok(removed)
    }

    /// builtin agent 数量
    pub fn builtin_count(&self) -> usize {
        let guard = self.inner.lock();
        guard
            .values()
            .flat_map(|versions| versions.values())
            .filter(|m| m.install_type == InstallType::Builtin)
            .count()
    }

    /// 总数（unique agent_id 数量）
    pub fn total(&self) -> usize {
        self.inner.lock().len()
    }

    fn registry_path(&self) -> PathBuf {
        self.path_manager.registry_path()
    }

    fn save_to_disk(&self) -> AgentMgmtResult<()> {
        let snapshot: Vec<AgentManifest> = {
            let guard = self.inner.lock();
            let mut v: Vec<AgentManifest> = guard
                .values()
                .flat_map(|versions| versions.values().cloned())
                .collect();
            v.sort_by(|a, b| {
                a.agent_id.cmp(&b.agent_id).then_with(|| {
                    version_util::compare_versions(
                        a.version.as_deref().unwrap_or("0.0.0"),
                        b.version.as_deref().unwrap_or("0.0.0"),
                    )
                    .unwrap_or(Ordering::Equal)
                })
            });
            v
        };
        let json = serde_json::to_string_pretty(&snapshot)?;
        let path = self.registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 原子写:tmp → rename
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        if std::fs::rename(&tmp, &path).is_err() {
            std::fs::copy(&tmp, &path)?;
            let _ = std::fs::remove_file(&tmp);
        }
        info!(
            "[agent_mgmt] Registry persisted: path={}, count={}",
            path.display(),
            snapshot.len()
        );
        Ok(())
    }

    /// 从磁盘读取，按 (agent_id, version) 分组
    fn read_from_disk(
        path: &std::path::Path,
    ) -> AgentMgmtResult<HashMap<String, HashMap<String, AgentManifest>>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let data = std::fs::read_to_string(path)?;
        if data.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let manifests: Vec<AgentManifest> = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[agent_mgmt] Registry parse error, backing up corrupt file: path={}, error={}",
                    path.display(),
                    e
                );
                // Backup corrupt file for forensic analysis
                let backup = path.with_extension("json.corrupt");
                if std::fs::rename(path, &backup)
                    .or_else(|_| std::fs::copy(path, &backup).map(|_| ()))
                    .is_err()
                {
                    warn!(
                        "[agent_mgmt] Failed to backup corrupt registry: path={}",
                        path.display()
                    );
                }
                return Ok(HashMap::new());
            }
        };
        let mut map: HashMap<String, HashMap<String, AgentManifest>> = HashMap::new();
        for m in manifests {
            let version_str = m.version.as_deref().unwrap_or("0.0.0");
            match version_util::normalize_version(version_str) {
                Ok(vkey) => {
                    map.entry(m.agent_id.clone()).or_default().insert(vkey, m);
                }
                Err(e) => {
                    warn!(
                        "[agent_mgmt] Skipping manifest with invalid version: agent_id={}, version={}, error={}",
                        m.agent_id, version_str, e
                    );
                }
            }
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
        let _ = std::fs::remove_dir_all(&dir);
        PathManager::new_with_root(dir)
    }

    /// 创建测试用 manifest，binary_path 指向临时目录内的真实路径
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

    #[test]
    fn insert_list_get_remove() {
        let pm = temp_pm();
        let r = AgentRegistry::empty(pm);
        assert_eq!(r.total(), 0);

        r.insert(sample_manifest("codex-acp")).unwrap();
        r.insert(sample_manifest("kimi-cli")).unwrap();
        assert_eq!(r.total(), 2);

        let got = r.get("codex-acp").expect("should exist");
        assert_eq!(got.agent_id, "codex-acp");

        assert!(r.contains("kimi-cli"));
        assert!(!r.contains("ghost"));

        let removed = r.remove("kimi-cli").unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].agent_id, "kimi-cli");
        assert_eq!(r.total(), 1);
    }

    #[test]
    fn insert_rejects_duplicate_exact_version() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        let err = r
            .insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap_err();
        assert!(matches!(
            err,
            AgentMgmtError::VersionAlreadyInstalled { .. }
        ));
    }

    #[test]
    fn insert_allows_different_versions() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .unwrap();
        assert_eq!(r.total(), 1); // 同一个 agent_id
        assert_eq!(r.get_all_versions("codex-acp").len(), 2);
    }

    #[test]
    fn get_returns_latest_version() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "1.5.0"))
            .unwrap();

        let latest = r.get("codex-acp").unwrap();
        assert_eq!(latest.version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn get_version_returns_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .unwrap();

        let v1 = r.get_version("codex-acp", "1.0.0").unwrap();
        assert_eq!(v1.version.as_deref(), Some("1.0.0"));

        let v2 = r.get_version("codex-acp", "2.0.0").unwrap();
        assert_eq!(v2.version.as_deref(), Some("2.0.0"));

        assert!(r.get_version("codex-acp", "3.0.0").is_none());
    }

    #[test]
    fn contains_version_checks_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();

        assert!(r.contains("codex-acp"));
        assert!(r.contains_version("codex-acp", "1.0.0"));
        assert!(!r.contains_version("codex-acp", "2.0.0"));
        assert!(!r.contains("ghost"));
    }

    #[test]
    fn remove_version_removes_specific() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .unwrap();

        let removed = r.remove_version("codex-acp", "1.0.0").unwrap();
        assert_eq!(removed.version.as_deref(), Some("1.0.0"));
        assert_eq!(r.total(), 1);
        assert!(r.contains_version("codex-acp", "2.0.0"));
        assert!(!r.contains_version("codex-acp", "1.0.0"));
    }

    #[test]
    fn remove_removes_all_versions() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest_with_version("codex-acp", "1.0.0"))
            .unwrap();
        r.insert(sample_manifest_with_version("codex-acp", "2.0.0"))
            .unwrap();

        let removed = r.remove("codex-acp").unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(r.total(), 0);
        assert!(!r.contains("codex-acp"));
    }

    #[test]
    fn remove_unknown_returns_not_found() {
        let r = AgentRegistry::empty(temp_pm());
        let err = r.remove("ghost").unwrap_err();
        assert!(matches!(err, AgentMgmtError::NotFound(_)));
    }

    #[test]
    fn upsert_overwrites() {
        let r = AgentRegistry::empty(temp_pm());
        r.upsert(sample_manifest_with_version("a", "1.0.0"))
            .unwrap();
        r.upsert(sample_manifest_with_version("a", "1.0.0"))
            .unwrap();
        assert_eq!(r.total(), 1);
    }

    #[test]
    fn insert_rejects_invalid_manifest() {
        let r = AgentRegistry::empty(temp_pm());
        let mut m = sample_manifest("../bad");
        m.installed_at = 0;
        let err = r.insert(m).unwrap_err();
        assert!(matches!(err, AgentMgmtError::InvalidManifest(_)));
    }

    #[test]
    fn load_persists_and_reloads() {
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
        .unwrap();
        r1.insert(sample_manifest_with_version_in(
            "alpha",
            "2.0.0",
            &install_dir,
        ))
        .unwrap();
        r1.insert(sample_manifest_with_version_in(
            "beta",
            "1.0.0",
            &install_dir,
        ))
        .unwrap();

        // 重新加载
        let r2 = AgentRegistry::load(pm).unwrap();
        assert_eq!(r2.total(), 2);
        assert!(r2.contains("alpha"));
        assert!(r2.contains("beta"));
        assert_eq!(r2.get_all_versions("alpha").len(), 2);
    }

    #[test]
    fn list_filters_builtin() {
        let pm = temp_pm();
        let r = AgentRegistry::empty(pm);
        r.insert(sample_manifest_with_version("user-1", "1.0.0"))
            .unwrap();
        let mut builtin = sample_manifest_with_version("builtin-1", "1.0.0");
        builtin.install_type = InstallType::Builtin;
        r.insert(builtin).unwrap();

        let user_only = r.list();
        assert_eq!(user_only.len(), 1);
        assert_eq!(user_only[0].agent_id, "user-1");
    }

    #[test]
    fn compare_versions_basic() {
        use std::cmp::Ordering;
        let cv = version_util::compare_versions;
        assert_eq!(cv("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(cv("1.0.0", "1.0.1").unwrap(), Ordering::Less);
        assert_eq!(cv("1.0.1", "1.0.0").unwrap(), Ordering::Greater);
        assert_eq!(cv("1.0.0", "2.0.0").unwrap(), Ordering::Less);
        assert_eq!(cv("1.2.3", "1.2.4").unwrap(), Ordering::Less);
    }

    #[test]
    fn compare_versions_with_v_prefix() {
        use std::cmp::Ordering;
        let cv = version_util::compare_versions;
        assert_eq!(cv("v1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(cv("V2.0.0", "1.9.9").unwrap(), Ordering::Greater);
    }

    #[test]
    fn compare_versions_returns_err_on_invalid() {
        assert!(version_util::compare_versions("invalid", "0.0.0").is_err());
    }

    #[test]
    fn version_key_normalizes() {
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

    #[test]
    fn normalize_platform_key_amd64() {
        assert_eq!(normalize_platform_key("linux", "amd64"), "linux-x86_64");
        assert_eq!(normalize_platform_key("linux", "x86_64"), "linux-x86_64");
    }

    #[test]
    fn normalize_platform_key_arm64() {
        assert_eq!(normalize_platform_key("linux", "arm64"), "linux-arm64");
        assert_eq!(normalize_platform_key("linux", "aarch64"), "linux-arm64");
        assert_eq!(normalize_platform_key("darwin", "arm64"), "darwin-arm64");
    }
}

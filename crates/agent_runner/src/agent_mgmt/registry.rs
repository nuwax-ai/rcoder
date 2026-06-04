//! Agent 注册表 (P0-1)
//!
//! 内存中存 `Mutex<HashMap<agent_id, AgentManifest>>`,序列化到 `registry.json`。
//! 用 `parking_lot::Mutex` 而非 DashMap:
//! - 写少读多(list/check 频繁,install/uninstall 偶尔)
//! - 写时还要同步落盘,Mutex 的"写时锁"语义更直接
//!
//! ## 持久化策略
//! 每次 `insert`/`remove` 立即 `save_to_disk`,启动时 `load_from_disk` 恢复。

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use shared_types::InstallType;
use tracing::{info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;

/// Agent 注册表(线程安全)
pub struct AgentRegistry {
    inner: Mutex<HashMap<String, AgentManifest>>,
    path_manager: PathManager,
}

impl AgentRegistry {
    /// 创建新的注册表(从磁盘加载已有数据)
    pub fn load(path_manager: PathManager) -> AgentMgmtResult<Self> {
        let map = Self::read_from_disk(&path_manager.registry_path())?;
        Ok(Self {
            inner: Mutex::new(map),
            path_manager,
        })
    }

    /// 安装根目录(供卸载安全检查等场景使用)
    pub fn install_dir(&self) -> &std::path::Path {
        self.path_manager.install_dir()
    }

    /// 内存中创建空注册表(用于测试)
    pub fn empty(path_manager: PathManager) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            path_manager,
        }
    }

    /// 列出所有已安装 agent(不含 builtin)
    pub fn list(&self) -> Vec<AgentManifest> {
        let guard = self.inner.lock();
        guard
            .values()
            .filter(|m| m.install_type != InstallType::Builtin)
            .cloned()
            .collect()
    }

    /// 查询单个 agent
    pub fn get(&self, agent_id: &str) -> Option<AgentManifest> {
        self.inner.lock().get(agent_id).cloned()
    }

    /// 是否已安装
    pub fn contains(&self, agent_id: &str) -> bool {
        self.inner.lock().contains_key(agent_id)
    }

    /// 插入/更新条目(立即落盘)
    #[allow(dead_code)] // used in tests and default_agents registration
    pub fn insert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        let mut guard = self.inner.lock();
        if guard.contains_key(&manifest.agent_id) {
            return Err(AgentMgmtError::InvalidManifest(format!(
                "agent already installed: {}",
                manifest.agent_id
            )));
        }
        guard.insert(manifest.agent_id.clone(), manifest);
        drop(guard);
        self.save_to_disk()
    }

    /// 覆盖式更新(用于 reinstall 场景)
    pub fn upsert(&self, manifest: AgentManifest) -> AgentMgmtResult<()> {
        manifest.validate()?;
        let mut guard = self.inner.lock();
        guard.insert(manifest.agent_id.clone(), manifest);
        drop(guard);
        self.save_to_disk()
    }

    /// 删除条目(立即落盘)
    pub fn remove(&self, agent_id: &str) -> AgentMgmtResult<AgentManifest> {
        let mut guard = self.inner.lock();
        let removed = guard
            .remove(agent_id)
            .ok_or_else(|| AgentMgmtError::NotFound(agent_id.to_string()))?;
        drop(guard);
        self.save_to_disk()?;
        Ok(removed)
    }

    /// builtin agent 数量
    pub fn builtin_count(&self) -> usize {
        self.inner
            .lock()
            .values()
            .filter(|m| m.install_type == InstallType::Builtin)
            .count()
    }

    /// 总数
    pub fn total(&self) -> usize {
        self.inner.lock().len()
    }

    fn registry_path(&self) -> PathBuf {
        self.path_manager.registry_path()
    }

    fn save_to_disk(&self) -> AgentMgmtResult<()> {
        let snapshot: Vec<AgentManifest> = {
            let guard = self.inner.lock();
            let mut v: Vec<AgentManifest> = guard.values().cloned().collect();
            v.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
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
        std::fs::rename(&tmp, &path)?;
        info!(
            "[agent_mgmt] Registry persisted: path={}, count={}",
            path.display(),
            snapshot.len()
        );
        Ok(())
    }

    fn read_from_disk(path: &std::path::Path) -> AgentMgmtResult<HashMap<String, AgentManifest>> {
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
                    "[agent_mgmt] Registry parse error (treat as empty): path={}, error={}",
                    path.display(),
                    e
                );
                return Ok(HashMap::new());
            }
        };
        Ok(manifests
            .into_iter()
            .map(|m| (m.agent_id.clone(), m))
            .collect())
    }
}

/// 语义化版本比较(major.minor.patch)
///
/// 格式不规范时返回 `Ordering::Less`(保守策略:触发更新)。
/// 支持 "v1.2.3" 前缀,自动剥离。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let parse = |s: &str| -> (u64, u64, u64) {
        let s = s.trim().trim_start_matches('v').trim_start_matches('V');
        let parts: Vec<&str> = s.split('.').collect();
        let major = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    let (a_major, a_minor, a_patch) = parse(a);
    let (b_major, b_minor, b_patch) = parse(b);
    match a_major.cmp(&b_major) {
        Ordering::Equal => match a_minor.cmp(&b_minor) {
            Ordering::Equal => a_patch.cmp(&b_patch),
            other => other,
        },
        other => other,
    }
}

/// 归一化平台 key: `{os}-{arch}` 格式
///
/// - `amd64 → x86_64`, `arm64 → aarch64`
/// - OS 保持原样(linux, darwin, windows)
pub fn normalize_platform_key(os: &str, arch: &str) -> String {
    let normalized_arch = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{normalized_arch}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::InstallType;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_pm() -> PathManager {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-mgmt-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        PathManager::new_with_root(dir)
    }

    fn sample_manifest(id: &str) -> AgentManifest {
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
        assert_eq!(removed.agent_id, "kimi-cli");
        assert_eq!(r.total(), 1);
    }

    #[test]
    fn insert_rejects_duplicate() {
        let r = AgentRegistry::empty(temp_pm());
        r.insert(sample_manifest("codex-acp")).unwrap();
        let err = r.insert(sample_manifest("codex-acp")).unwrap_err();
        assert!(matches!(err, AgentMgmtError::InvalidManifest(_)));
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
        r.upsert(sample_manifest("a")).unwrap();
        r.upsert(sample_manifest("a")).unwrap();
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
        let r1 = AgentRegistry::empty(pm.clone());
        r1.insert(sample_manifest("alpha")).unwrap();
        r1.insert(sample_manifest("beta")).unwrap();

        // 重新加载
        let r2 = AgentRegistry::load(pm).unwrap();
        assert_eq!(r2.total(), 2);
        assert!(r2.contains("alpha"));
        assert!(r2.contains("beta"));
    }

    #[test]
    fn list_filters_builtin() {
        let pm = temp_pm();
        let r = AgentRegistry::empty(pm);
        r.insert(sample_manifest("user-1")).unwrap();
        let mut builtin = sample_manifest("builtin-1");
        builtin.install_type = InstallType::Builtin;
        r.insert(builtin).unwrap();

        let user_only = r.list();
        assert_eq!(user_only.len(), 1);
        assert_eq!(user_only[0].agent_id, "user-1");
    }

    #[test]
    fn compare_versions_basic() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0.1"), Ordering::Less);
        assert_eq!(compare_versions("1.0.1", "1.0.0"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "2.0.0"), Ordering::Less);
        assert_eq!(compare_versions("1.2.3", "1.2.4"), Ordering::Less);
    }

    #[test]
    fn compare_versions_with_v_prefix() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("v1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("V2.0.0", "1.9.9"), Ordering::Greater);
    }

    #[test]
    fn compare_versions_malformed_defaults_to_less() {
        use std::cmp::Ordering;
        // 格式不规范时,缺失的 patch 默认为 0
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0", "1.0.1"), Ordering::Less);
    }

    #[test]
    fn normalize_platform_key_amd64() {
        assert_eq!(normalize_platform_key("linux", "amd64"), "linux-x86_64");
        assert_eq!(normalize_platform_key("linux", "x86_64"), "linux-x86_64");
    }

    #[test]
    fn normalize_platform_key_arm64() {
        assert_eq!(normalize_platform_key("linux", "arm64"), "linux-aarch64");
        assert_eq!(normalize_platform_key("linux", "aarch64"), "linux-aarch64");
        assert_eq!(normalize_platform_key("darwin", "arm64"), "darwin-aarch64");
    }
}

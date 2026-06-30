//! Agent 卸载
//!
//! 支持两种模式：
//! - 卸载全部版本：删除整个 agent 目录 + 注册表移除所有版本
//! - 卸载指定版本：只删除版本目录 + 注册表移除单个版本

use std::path::Path;

use shared_types::InstallType;
use tracing::{info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::registry::AgentRegistry;

/// 统一卸载入口
///
/// - `version = None` → 卸载全部版本（向后兼容）
/// - `version = Some(v)` → 只卸载指定版本
pub async fn uninstall_with_version(
    registry: &AgentRegistry,
    agent_id: &str,
    version: Option<&str>,
) -> AgentMgmtResult<Vec<AgentManifest>> {
    match version {
        Some(v) => {
            let m = uninstall_version(registry, agent_id, v).await?;
            Ok(vec![m])
        }
        None => uninstall(registry, agent_id).await,
    }
}

/// 卸载全部版本
///
/// 删除整个 `{install_dir}/{agent_id}/` 目录，从注册表移除所有版本。
pub async fn uninstall(
    registry: &AgentRegistry,
    agent_id: &str,
) -> AgentMgmtResult<Vec<AgentManifest>> {
    let manifest = registry
        .get(agent_id)
        .ok_or_else(|| AgentMgmtError::NotFound(agent_id.to_string()))?;

    // builtin 保护
    if manifest.install_type == InstallType::Builtin {
        return Err(AgentMgmtError::BuiltinProtected);
    }

    // 安全检查: binary_path 必须在安装目录下
    validate_binary_path(&manifest.binary_path, registry.install_dir())?;

    // 删除整个 agent 目录（包含所有版本）
    let agent_dir = registry.install_dir().join(agent_id);
    if agent_dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(&agent_dir).await
    {
        warn!(
            "[agent_mgmt] Failed to remove agent directory: path={}, error={}",
            agent_dir.display(),
            e
        );
    }

    // 从注册表移除（所有版本）
    let removed = registry.remove(agent_id)?;
    info!(
        "[agent_mgmt] Uninstalled all versions: agent_id={}, count={}",
        agent_id,
        removed.len()
    );
    Ok(removed)
}

/// 卸载指定版本
///
/// 只删除 `{install_dir}/{agent_id}/{version}/` 目录。
/// 如果删除后 agent 没有任何版本了，自动清理空的父目录。
pub async fn uninstall_version(
    registry: &AgentRegistry,
    agent_id: &str,
    version: &str,
) -> AgentMgmtResult<AgentManifest> {
    let manifest = registry
        .get_version(agent_id, version)
        .ok_or_else(|| AgentMgmtError::NotFound(format!("{}@{}", agent_id, version)))?;

    // builtin 保护
    if manifest.install_type == InstallType::Builtin {
        return Err(AgentMgmtError::BuiltinProtected);
    }

    // 安全检查: binary_path 必须在安装目录下
    validate_binary_path(&manifest.binary_path, registry.install_dir())?;

    // 只删除版本目录: {install_dir}/{agent_id}/{version}/
    let version_dir = registry
        .path_manager()
        .agent_version_dir(agent_id, version)
        .map_err(AgentMgmtError::InvalidManifest)?;

    if version_dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(&version_dir).await
    {
        warn!(
            "[agent_mgmt] Failed to remove version directory: path={}, error={}",
            version_dir.display(),
            e
        );
    }

    // 从注册表移除指定版本
    let removed = registry.remove_version(agent_id, version)?;

    // 如果 agent 没有剩余版本，清理空的父目录
    if !registry.contains(agent_id) {
        let agent_dir = registry
            .path_manager()
            .agent_dir(agent_id)
            .map_err(AgentMgmtError::InvalidManifest)?;
        // best-effort: remove_dir 只能删空目录
        let _ = tokio::fs::remove_dir(&agent_dir).await;
    }

    info!(
        "[agent_mgmt] Uninstalled version: agent_id={}, version={}",
        agent_id, version
    );
    Ok(removed)
}

/// 校验 binary_path 在安装目录内（防止路径遍历删除系统文件）
fn validate_binary_path(binary_path: &str, install_dir: &Path) -> AgentMgmtResult<()> {
    let binary_path = std::path::PathBuf::from(binary_path);
    let within_install = match (binary_path.canonicalize(), install_dir.canonicalize()) {
        (Ok(canon_bin), Ok(canon_inst)) => canon_bin.starts_with(&canon_inst),
        _ => binary_path.starts_with(install_dir),
    };
    if !within_install {
        return Err(AgentMgmtError::InvalidManifest(format!(
            "binary_path '{}' is outside install_dir '{}'",
            binary_path.display(),
            install_dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::InstallType;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_pm() -> crate::agent_mgmt::path_manager::PathManager {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-mgmt-uninstall-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        crate::agent_mgmt::path_manager::PathManager::new_with_root(dir)
    }

    fn sample(id: &str, install_type: InstallType, install_dir: &std::path::Path) -> AgentManifest {
        let binary_path = install_dir.join("bin").join(id);
        let mut m = AgentManifest::new(
            id.into(),
            install_type,
            "fake".into(),
            vec![],
            binary_path.to_string_lossy().to_string(),
            0,
            "executable".into(),
        );
        m.installed_at = 0;
        m
    }

    fn sample_with_version(
        id: &str,
        version: &str,
        install_type: InstallType,
        install_dir: &std::path::Path,
    ) -> AgentManifest {
        let binary_path = install_dir.join(id).join(version).join(id);
        let mut m = AgentManifest::new(
            id.into(),
            install_type,
            "fake".into(),
            vec![],
            binary_path.to_string_lossy().to_string(),
            0,
            "executable".into(),
        );
        m.version = Some(version.to_string());
        m.installed_at = 0;
        m
    }

    // === 全量卸载测试 ===

    #[tokio::test]
    async fn builtin_uninstall_rejected() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample("builtin-1", InstallType::Builtin, &install_dir))
            .unwrap();
        let err = uninstall(&r, "builtin-1").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::BuiltinProtected));
        assert!(r.contains("builtin-1"));
    }

    #[tokio::test]
    async fn user_install_uninstall_succeeds() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample("user-1", InstallType::Binary, &install_dir))
            .unwrap();
        let removed = uninstall(&r, "user-1").await.unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].agent_id, "user-1");
        assert!(!r.contains("user-1"));
    }

    #[tokio::test]
    async fn uninstall_rejects_binary_outside_install_dir() {
        let r = AgentRegistry::empty(temp_pm());
        let mut m = sample(
            "evil",
            InstallType::Binary,
            &std::path::PathBuf::from("/tmp"),
        );
        m.binary_path = "/etc/passwd".to_string();
        r.insert(m).unwrap();
        let err = uninstall(&r, "evil").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn unknown_uninstall_returns_not_found() {
        let r = AgentRegistry::empty(temp_pm());
        let err = uninstall(&r, "ghost").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::NotFound(_)));
    }

    // === Per-version 卸载测试 ===

    #[tokio::test]
    async fn uninstall_single_version_succeeds() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();

        // 创建两个版本的目录和文件
        let v1_dir = install_dir.join("test-agent").join("1.0.0");
        let v2_dir = install_dir.join("test-agent").join("2.0.0");
        std::fs::create_dir_all(&v1_dir).unwrap();
        std::fs::create_dir_all(&v2_dir).unwrap();
        std::fs::write(v1_dir.join("test-agent"), "v1").unwrap();
        std::fs::write(v2_dir.join("test-agent"), "v2").unwrap();

        r.insert(sample_with_version(
            "test-agent",
            "1.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();
        r.insert(sample_with_version(
            "test-agent",
            "2.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();

        // 卸载 1.0.0
        let removed = uninstall_version(&r, "test-agent", "1.0.0").await.unwrap();
        assert_eq!(removed.version, Some("1.0.0".to_string()));

        // 2.0.0 仍然存在
        assert!(r.contains("test-agent"));
        assert!(r.get_version("test-agent", "2.0.0").is_some());
        assert!(r.get_version("test-agent", "1.0.0").is_none());

        // 1.0.0 目录已删除，2.0.0 目录还在
        assert!(!v1_dir.exists());
        assert!(v2_dir.exists());
    }

    #[tokio::test]
    async fn uninstall_last_version_cleans_parent_dir() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();

        // 创建只有一个版本
        let v_dir = install_dir.join("solo-agent").join("1.0.0");
        std::fs::create_dir_all(&v_dir).unwrap();
        std::fs::write(v_dir.join("solo-agent"), "v1").unwrap();

        r.insert(sample_with_version(
            "solo-agent",
            "1.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();

        let removed = uninstall_version(&r, "solo-agent", "1.0.0").await.unwrap();
        assert_eq!(removed.version, Some("1.0.0".to_string()));

        // agent 目录应该被清理
        assert!(!r.contains("solo-agent"));
        let agent_dir = install_dir.join("solo-agent");
        assert!(!agent_dir.exists());
    }

    #[tokio::test]
    async fn uninstall_nonexistent_version_returns_not_found() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample_with_version(
            "test-agent",
            "1.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();

        let err = uninstall_version(&r, "test-agent", "9.9.9")
            .await
            .unwrap_err();
        assert!(matches!(err, AgentMgmtError::NotFound(_)));
    }

    #[tokio::test]
    async fn uninstall_version_builtin_rejected() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample_with_version(
            "builtin-1",
            "1.0.0",
            InstallType::Builtin,
            &install_dir,
        ))
        .unwrap();

        let err = uninstall_version(&r, "builtin-1", "1.0.0")
            .await
            .unwrap_err();
        assert!(matches!(err, AgentMgmtError::BuiltinProtected));
    }

    #[tokio::test]
    async fn uninstall_version_rejects_binary_outside_install_dir() {
        let r = AgentRegistry::empty(temp_pm());
        let mut m = sample_with_version(
            "evil",
            "1.0.0",
            InstallType::Binary,
            &std::path::PathBuf::from("/tmp"),
        );
        m.binary_path = "/etc/passwd".to_string();
        r.insert(m).unwrap();

        let err = uninstall_version(&r, "evil", "1.0.0").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn uninstall_with_version_none_uninstalls_all() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();

        let v1_dir = install_dir.join("multi-agent").join("1.0.0");
        let v2_dir = install_dir.join("multi-agent").join("2.0.0");
        std::fs::create_dir_all(&v1_dir).unwrap();
        std::fs::create_dir_all(&v2_dir).unwrap();

        r.insert(sample_with_version(
            "multi-agent",
            "1.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();
        r.insert(sample_with_version(
            "multi-agent",
            "2.0.0",
            InstallType::Binary,
            &install_dir,
        ))
        .unwrap();

        // version = None → 卸载全部
        let removed = uninstall_with_version(&r, "multi-agent", None)
            .await
            .unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!r.contains("multi-agent"));
    }
}

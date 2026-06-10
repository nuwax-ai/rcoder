//! Agent 卸载 (P0-1)
//!
//! 行为:
//! - 拒绝卸载 builtin agent
//! - 删除 manifest 指定的入口文件(若存在)
//! - 删除 agent 自己的子目录(若存在,避免 tar/zip 解压残留)
//! - 从注册表移除条目（所有版本）

use shared_types::InstallType;
use tracing::{info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::registry::AgentRegistry;

/// 执行卸载（删除所有版本）
///
/// 支持两种 binary_path 类型：
/// - 目录型（directory-based agent）：binary_path 指向 agent 安装目录，直接 `remove_dir_all`
/// - 文件型（binary agent）：binary_path 指向入口文件，删除文件后清理父目录
pub async fn uninstall(registry: &AgentRegistry, agent_id: &str) -> AgentMgmtResult<Vec<AgentManifest>> {
    let manifest = registry
        .get(agent_id)
        .ok_or_else(|| AgentMgmtError::NotFound(agent_id.to_string()))?;

    // builtin 保护
    if manifest.install_type == InstallType::Builtin {
        return Err(AgentMgmtError::BuiltinProtected);
    }

    // 0. 安全检查:binary_path 必须在安装目录下(防止 manifest 被篡改后删除系统文件)
    let binary_path = std::path::PathBuf::from(&manifest.binary_path);
    let install_dir = registry.install_dir();
    // 尝试 canonicalize 防止 ../.. 绕过;回退到原始路径比较(兼容路径不存在的情况)
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

    // 1. 删除整个 agent 目录（包含所有版本）
    let agent_dir = install_dir.join(agent_id);
    if agent_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&agent_dir).await {
            warn!(
                "[agent_mgmt] Failed to remove agent directory during uninstall: path={}, error={}",
                agent_dir.display(),
                e
            );
        }
    }

    // 2. 从注册表移除（所有版本）
    let removed = registry.remove(agent_id)?;
    let count = removed.len();
    info!(
        "[agent_mgmt] Uninstalled: agent_id={}, versions_count={}",
        agent_id, count
    );
    Ok(removed)
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

    #[tokio::test]
    async fn builtin_uninstall_rejected() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample("builtin-1", InstallType::Builtin, &install_dir)).unwrap();
        let err = uninstall(&r, "builtin-1").await.unwrap_err();
        assert!(matches!(err, AgentMgmtError::BuiltinProtected));
        // builtin 仍应在注册表
        assert!(r.contains("builtin-1"));
    }

    #[tokio::test]
    async fn user_install_uninstall_succeeds() {
        let r = AgentRegistry::empty(temp_pm());
        let install_dir = r.install_dir().to_path_buf();
        r.insert(sample("user-1", InstallType::Binary, &install_dir)).unwrap();
        let removed = uninstall(&r, "user-1").await.unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].agent_id, "user-1");
        assert!(!r.contains("user-1"));
    }

    #[tokio::test]
    async fn uninstall_rejects_binary_outside_install_dir() {
        let r = AgentRegistry::empty(temp_pm());
        let mut m = sample("evil", InstallType::Binary, &std::path::PathBuf::from("/tmp"));
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
}

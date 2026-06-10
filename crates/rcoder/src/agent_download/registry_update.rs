//! Registry 更新函数
//!
//! 用于 rcoder 更新 agent-runner 的 registry.json 文件。
//! 使用与 agent_runner 兼容的 JSON 格式。

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::info;

use super::error::AgentDownloadError;

/// Agent Manifest（与 agent_runner 兼容的格式）
///
/// 注意：install_type 使用字符串序列化，与 agent_runner 的 InstallType 枚举兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent_id: String,
    pub install_type: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub binary_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub file_size: u64,
    pub file_type: String,
    pub installed_at: i64,
}

/// 更新 agent-runner 的 registry.json
///
/// 将新安装的 agent 信息写入 registry，支持多版本。
/// 写入的格式与 agent_runner 的 Vec<AgentManifest> 兼容。
///
/// # 并发安全
///
/// 使用原子写入（tmp + rename）保证文件完整性。
/// 在单实例部署场景下是安全的。
/// 多实例并发写入时，后写入的会覆盖先写入的（可能丢失更新），
/// 但不会导致文件损坏。如需多实例完全安全，应引入文件锁。
pub fn update_registry(
    acp_agent_dir: &Path,
    agent_id: &str,
    version: &str,
    command: &str,
    args: &[String],
) -> Result<(), AgentDownloadError> {
    let registry_path = acp_agent_dir.join("registry.json");

    // 读取现有 registry（Vec<AgentManifest> 格式）
    let mut manifests: Vec<AgentManifest> = if registry_path.exists() {
        let data = std::fs::read_to_string(&registry_path)?;
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Vec::new()
    };

    // 检查是否已存在相同版本
    let existing = manifests.iter().position(|m| {
        m.agent_id == agent_id && m.version.as_deref() == Some(version)
    });

    // 创建新的 manifest
    let manifest = AgentManifest {
        agent_id: agent_id.to_string(),
        install_type: "url".to_string(),
        command: command.to_string(),
        args: args.to_vec(),
        binary_path: command.to_string(), // 相对路径
        source: None,
        version: Some(version.to_string()),
        file_size: 0,
        file_type: "binary".to_string(),
        installed_at: chrono::Utc::now().timestamp(),
    };

    // 更新或插入
    if let Some(idx) = existing {
        manifests[idx] = manifest;
        info!(
            agent_id = %agent_id,
            version = %version,
            "Updated existing registry entry"
        );
    } else {
        manifests.push(manifest);
        info!(
            agent_id = %agent_id,
            version = %version,
            "Added new registry entry"
        );
    }

    // 排序（便于阅读）
    manifests.sort_by(|a, b| {
        a.agent_id
            .cmp(&b.agent_id)
            .then_with(|| {
                a.version
                    .as_deref()
                    .unwrap_or("")
                    .cmp(&b.version.as_deref().unwrap_or(""))
            })
    });

    // 写入文件（原子写入）
    let json = serde_json::to_string_pretty(&manifests)?;
    let tmp_path = registry_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json.as_bytes())?;
    std::fs::rename(&tmp_path, &registry_path)?;

    info!(
        registry_path = %registry_path.display(),
        total = manifests.len(),
        "Registry persisted"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_registry_new_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        update_registry(acp_agent_dir, "test-agent", "1.0.0", "test-cmd", &[]).unwrap();

        // 验证文件存在
        let registry_path = acp_agent_dir.join("registry.json");
        assert!(registry_path.exists());

        // 验证内容
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].agent_id, "test-agent");
        assert_eq!(manifests[0].version, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_update_registry_add_version() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        // 添加第一个版本
        update_registry(acp_agent_dir, "test-agent", "1.0.0", "test-cmd", &[]).unwrap();

        // 添加第二个版本
        update_registry(acp_agent_dir, "test-agent", "2.0.0", "test-cmd", &[]).unwrap();

        // 验证有两个版本
        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 2);
    }

    #[test]
    fn test_update_registry_update_existing() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        // 添加版本
        update_registry(acp_agent_dir, "test-agent", "1.0.0", "old-cmd", &[]).unwrap();

        // 更新同一版本
        update_registry(acp_agent_dir, "test-agent", "1.0.0", "new-cmd", &["--flag".to_string()]).unwrap();

        // 验证只有一个版本，但命令已更新
        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].command, "new-cmd");
        assert_eq!(manifests[0].args, vec!["--flag".to_string()]);
    }

    #[test]
    fn test_update_registry_multiple_agents() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        update_registry(acp_agent_dir, "agent-a", "1.0.0", "cmd-a", &[]).unwrap();
        update_registry(acp_agent_dir, "agent-b", "1.0.0", "cmd-b", &[]).unwrap();

        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 2);

        // 验证按 agent_id 排序
        assert_eq!(manifests[0].agent_id, "agent-a");
        assert_eq!(manifests[1].agent_id, "agent-b");
    }
}

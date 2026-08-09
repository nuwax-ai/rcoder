//! Registry 更新函数
//!
//! 更新安装目录下的 registry.json 文件。
//! 使用与 agent_runner 兼容的 JSON 格式。

use std::fs::OpenOptions;
use std::path::Path;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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

/// 更新安装目录下的 registry.json
///
/// 将新安装的 agent 信息写入 registry，支持多版本。
/// 写入的格式与 agent_runner 的 Vec<AgentManifest> 兼容。
///
/// # 并发安全
///
/// 使用文件锁串行化 read-modify-write，再通过 tmp + rename 原子替换。
/// 同一共享目录被多个进程更新时也不会丢失已经写入的条目。
pub async fn update_registry(
    acp_agent_dir: &Path,
    agent_id: &str,
    version: &str,
    command: &str,
    args: &[String],
) -> Result<(), AgentDownloadError> {
    let acp_agent_dir = acp_agent_dir.to_path_buf();
    let agent_id = agent_id.to_string();
    let version = version.to_string();
    let command = command.to_string();
    let args = args.to_vec();

    tokio::task::spawn_blocking(move || {
        update_registry_sync(&acp_agent_dir, &agent_id, &version, &command, &args)
    })
    .await
    .map_err(|e| AgentDownloadError::Io(std::io::Error::other(e.to_string())))?
}

fn update_registry_sync(
    acp_agent_dir: &Path,
    agent_id: &str,
    version: &str,
    command: &str,
    args: &[String],
) -> Result<(), AgentDownloadError> {
    let lock_path = acp_agent_dir.join(".registry.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    FileExt::lock_exclusive(&lock_file)?;

    let registry_path = acp_agent_dir.join("registry.json");

    // 读取现有 registry（Vec<AgentManifest> 格式）
    let mut manifests: Vec<AgentManifest> = if registry_path.exists() {
        let data = std::fs::read_to_string(&registry_path)?;
        // 解析失败必须上抛而非清空：unwrap_or_default() 会把损坏的 registry.json 当成空 Vec，
        // 随后只写回当前单条记录，擦除所有历史 agent 注册记录（数据丢失）。
        serde_json::from_str(&data)?
    } else {
        Vec::new()
    };

    // 检查是否已存在相同版本
    let existing = manifests
        .iter()
        .position(|m| m.agent_id == agent_id && m.version.as_deref() == Some(version));

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
        a.agent_id.cmp(&b.agent_id).then_with(|| {
            a.version
                .as_deref()
                .unwrap_or("")
                .cmp(b.version.as_deref().unwrap_or(""))
        })
    });

    // 写入文件（原子写入）
    let json = serde_json::to_string_pretty(&manifests)?;
    let tmp_path = registry_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json.as_bytes())?;
    if std::fs::rename(&tmp_path, &registry_path).is_err() {
        std::fs::copy(&tmp_path, &registry_path)?;
        if let Err(e) = std::fs::remove_file(&tmp_path) {
            warn!(
                "[registry] failed to clean up temp file {}: {e}",
                tmp_path.display()
            );
        }
    }

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

    #[tokio::test]
    async fn test_update_registry_new_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        update_registry(acp_agent_dir, "test-agent", "1.0.0", "test-cmd", &[])
            .await
            .unwrap();

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_updates_do_not_lose_entries() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let mut tasks = Vec::new();
        for index in 0..16 {
            let directory = tmp_dir.path().to_path_buf();
            tasks.push(tokio::spawn(async move {
                update_registry(&directory, &format!("agent-{index}"), "1.0.0", "agent", &[]).await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let data = std::fs::read_to_string(tmp_dir.path().join("registry.json")).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 16);
    }

    #[tokio::test]
    async fn test_update_registry_add_version() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        // 添加第一个版本
        update_registry(acp_agent_dir, "test-agent", "1.0.0", "test-cmd", &[])
            .await
            .unwrap();

        // 添加第二个版本
        update_registry(acp_agent_dir, "test-agent", "2.0.0", "test-cmd", &[])
            .await
            .unwrap();

        // 验证有两个版本
        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 2);
    }

    #[tokio::test]
    async fn test_update_registry_update_existing() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        // 添加版本
        update_registry(acp_agent_dir, "test-agent", "1.0.0", "old-cmd", &[])
            .await
            .unwrap();

        // 更新同一版本
        update_registry(
            acp_agent_dir,
            "test-agent",
            "1.0.0",
            "new-cmd",
            &["--flag".to_string()],
        )
        .await
        .unwrap();

        // 验证只有一个版本，但命令已更新
        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].command, "new-cmd");
        assert_eq!(manifests[0].args, vec!["--flag".to_string()]);
    }

    #[tokio::test]
    async fn test_update_registry_multiple_agents() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let acp_agent_dir = tmp_dir.path();

        update_registry(acp_agent_dir, "agent-a", "1.0.0", "cmd-a", &[])
            .await
            .unwrap();
        update_registry(acp_agent_dir, "agent-b", "1.0.0", "cmd-b", &[])
            .await
            .unwrap();

        let registry_path = acp_agent_dir.join("registry.json");
        let data = std::fs::read_to_string(&registry_path).unwrap();
        let manifests: Vec<AgentManifest> = serde_json::from_str(&data).unwrap();
        assert_eq!(manifests.len(), 2);

        // 验证按 agent_id 排序
        assert_eq!(manifests[0].agent_id, "agent-a");
        assert_eq!(manifests[1].agent_id, "agent-b");
    }
}

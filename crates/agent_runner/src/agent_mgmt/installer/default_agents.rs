//! Default (built-in) agents (P0-1)
//!
//! 启动时异步注册内置 agent,失败不阻塞主流程(降级为 warn 日志)。
//!
//! ## 内置清单
//! - `claude-code-acp` — 通过 npm 全局安装(@anthropic-ai/claude-code-acp)
//!
//! ## 行为
//! - 启动时 spawn 一个 `tokio::task`,不阻塞 `main`
//! - 已注册(`registry.contains(...)`)则跳过
//! - npm 不可用 / 网络失败时记 warn,不返回错误

use shared_types::InstallType;
use tracing::{info, warn};

use super::npm_installer;
use crate::agent_mgmt::error::AgentMgmtResult;
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// 内置 agent 配套元数据(供 install 时使用)
#[derive(Debug, Clone)]
pub struct DefaultAgentSpec {
    pub agent_id: String,
    pub package: String,
    pub command: String,
}

/// 默认 agent 的安装规格(`DefaultAgentInfo` 不含 package/command 字段,故另存)
pub fn list_default_specs() -> Vec<DefaultAgentSpec> {
    vec![DefaultAgentSpec {
        agent_id: "claude-code-acp".into(),
        package: "@anthropic-ai/claude-code-acp".into(),
        command: "claude-code-acp".into(),
    }]
}

/// 异步注册所有内置 agent(不阻塞)
pub fn spawn_registration(
    registry: std::sync::Arc<AgentRegistry>,
    path_manager: PathManager,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = register_all(&registry, &path_manager).await {
            warn!("[agent_mgmt] default agent registration failed: {e}");
        }
        // 不论 register_all 结果如何,只要内置 agent 已在注册表中,
        // 都要把它标记为 InstallType::Builtin 以防被卸载
        mark_builtin(&registry);
    })
}

async fn register_all(
    registry: &AgentRegistry,
    path_manager: &PathManager,
) -> AgentMgmtResult<()> {
    let specs = list_default_specs();
    for spec in &specs {
        if registry.contains(&spec.agent_id) {
            info!(
                "[agent_mgmt] default agent already registered: agent_id={}",
                spec.agent_id
            );
            continue;
        }
        match npm_installer::install_from_npm(
            registry,
            path_manager,
            &spec.agent_id,
            &spec.package,
            &spec.command,
        )
        .await
        {
            Ok(resp) => {
                info!(
                    "[agent_mgmt] default agent registered: agent_id={}, binary_path={}",
                    resp.agent_id, resp.binary_path
                );
            }
            Err(e) => {
                warn!(
                    "[agent_mgmt] default agent install failed (degraded, not fatal): agent_id={}, error={}",
                    spec.agent_id, e
                );
            }
        }
    }
    Ok(())
}

/// 把已注册的内置 agent 标记为 `InstallType::Builtin`(升级 install_type)
/// 注意:在 register_all 完成后调用,确保 builtin agent 永远不会被 uninstall
pub fn mark_builtin(registry: &AgentRegistry) {
    let specs = list_default_specs();
    for spec in &specs {
        if let Some(manifest) = registry.get(&spec.agent_id)
            && manifest.install_type != InstallType::Builtin {
                let mut updated = manifest.clone();
                updated.install_type = InstallType::Builtin;
                if let Err(e) = registry.upsert(updated) {
                    warn!(
                        "[agent_mgmt] failed to mark agent as builtin: agent_id={}, error={}",
                        spec.agent_id, e
                    );
                }
            }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_mgmt::installer::AgentManifest;
    use crate::agent_mgmt::path_manager::PathManager;
    use crate::agent_mgmt::registry::AgentRegistry;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_pm() -> PathManager {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-mgmt-default-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        PathManager::new_with_root(dir)
    }

    /// 验证:已注册的内置 agent 被 mark_builtin 后,install_type 变成 Builtin,
    /// 从此卸载时被 BuiltinProtected 拦截
    #[test]
    fn mark_builtin_upgrades_install_type() {
        let r = AgentRegistry::empty(temp_pm());

        // 模拟 npm 安装 claude-code-acp(初始 install_type=Npm)
        let mut m = AgentManifest::new(
            "claude-code-acp".into(),
            InstallType::Npm,
            "claude-code-acp".into(),
            vec![],
            "/usr/local/bin/claude-code-acp".into(),
            0,
            "symlink".into(),
        );
        m.installed_at = 12345;
        r.insert(m).unwrap();

        // 升级为 builtin
        mark_builtin(&r);

        // 验证 install_type 已升级(由此可推论:uninstaller 会拒绝)
        let got = r.get("claude-code-acp").unwrap();
        assert_eq!(got.install_type, InstallType::Builtin);
    }

    /// 验证:未注册的 agent 不被 mark_builtin 触碰
    #[test]
    fn mark_builtin_skips_unregistered() {
        let r = AgentRegistry::empty(temp_pm());
        // claude-code-acp 未注册,mark_builtin 应当 noop
        mark_builtin(&r);
        assert!(!r.contains("claude-code-acp"));
    }
}

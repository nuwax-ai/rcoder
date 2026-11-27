//! 容器内技能目录启动对账（codex 复审 P2）。
//!
//! RBD 独占卷挂载在 agent 容器内——主进程的 skill_sync_reconciler 只扫
//! 主进程工作区与共享 computer 根，触及不到已迁移用户的卷。harness 等目录
//! 约定升级后（如 `.codex` → `.nuwax-harness`），卷内 `.agents` 与目标
//! 目录的漂移由**实际持有卷的一侧**（本进程）在启动时补偿。
//!
//! 行为：一次性、后台、尽力而为——漂移检查失败或目录缺失只记录日志，
//! 绝不阻塞 agent 启动（对账是优化路径，不是启动前置）。

use std::path::{Path, PathBuf};

use tracing::{info, warn};

const MAX_SCAN_DEPTH: u32 = 3;
const RECONCILE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// 启动后台一次性对账（spawn 即返回，不阻塞启动）。
pub fn spawn_startup_reconcile() {
    tokio::spawn(async move {
        let roots = candidate_roots();
        info!(
            ?roots,
            "[SKILL_SYNC/EMBED] startup reconcile begin (container-side, RBD volumes included)"
        );
        let mut synced = 0u32;
        for root in roots {
            scan(&root, 0, &mut synced).await;
        }
        info!("[SKILL_SYNC/EMBED] startup reconcile done: synced={synced}");
    });
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // 与主进程 reconciler 同源的常量；容器内 computer 根 = RBD 卷挂载点。
    roots.push(PathBuf::from(shared_types::paths::WORKSPACE_ROOT));
    roots.push(PathBuf::from(shared_types::paths::COMPUTER_WORKSPACE_ROOT));
    // PROJECT_WORKSPACE_BASE 是容器形态的实际工作区（k8s_agent_env 注入），
    // 常量默认值与注入值不同时以 env 为准。
    if let Ok(base) = std::env::var("PROJECT_WORKSPACE_BASE") {
        let base = PathBuf::from(base.trim());
        if !base.as_os_str().is_empty() && !roots.contains(&base) {
            roots.push(base);
        }
    }
    roots
}

async fn scan(dir: &Path, depth: u32, synced: &mut u32) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let agents = dir.join(".agents");
    if tokio::fs::try_exists(&agents).await.unwrap_or(false) {
        reconcile_workspace(dir, synced).await;
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            Box::pin(scan(&entry.path(), depth + 1, synced)).await;
        }
    }
}

async fn reconcile_workspace(workspace: &Path, synced: &mut u32) {
    // 与主进程 reconcile_workspace 同款：有任一同步源(skills 或 agents——
    // 复审第四轮 P2: 仅有 .agents/agents 的工作区此前被跳过) + fanout 漂移
    // → 补同步。
    let has_skills = tokio::fs::try_exists(workspace.join(".agents/skills"))
        .await
        .unwrap_or(false);
    let has_agents = tokio::fs::try_exists(workspace.join(".agents/agents"))
        .await
        .unwrap_or(false);
    if !has_skills && !has_agents {
        return;
    }
    match file_server::fanout_names_drifted(workspace).await {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            warn!(
                "[SKILL_SYNC/EMBED] drift check {} failed: {error}",
                workspace.display()
            );
            return;
        }
    }
    match file_server::sync_agents_with_wait(workspace, RECONCILE_LOCK_WAIT).await {
        Ok(file_server::SyncAgentsOutcome::Completed) => {
            *synced += 1;
            info!(
                "[SKILL_SYNC/EMBED] synced {} (fanout names refreshed)",
                workspace.display()
            );
        }
        Ok(file_server::SyncAgentsOutcome::BusySkipped) => {
            info!(
                "[SKILL_SYNC/EMBED] workspace busy, skipped: {}",
                workspace.display()
            );
        }
        Err(error) => {
            warn!(
                "[SKILL_SYNC/EMBED] sync {} failed: {error}",
                workspace.display()
            );
        }
    }
}

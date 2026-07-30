//! 启动时后台 reconciler: 对 fan-out 版本落后的 workspace 自动补 sync_agents。
//!
//! 解决: fan-out 列表扩展 (加 grok/pi/cursor...) 后, 已存在的旧 workspace 缺新 agent 目录,
//! 靠手动 push-skills 逐个补齐不可行。本模块启动时后台遍历所有 workspace, 版本 marker
//! 驱动 (`.agents/.sync_version`) 自动补齐, 已同步的 O(1) 跳过。
//!
//! 复用 `batch_migrate` 模式 (spawn 后台 + 遍历 + 幂等 marker + 失败只 warn 不阻断)。
//! env `RCODER_SKILL_SYNC_RECONCILE_ON_STARTUP` 控制 (默认 true, 因轻量)。

use std::path::{Path, PathBuf};

use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};
use tracing::{info, warn};

/// env 开关名 (默认 true: 版本 O(1) 跳过, 日常启动几乎零开销)。
const ENV_ENABLED: &str = "RCODER_SKILL_SYNC_RECONCILE_ON_STARTUP";

/// 递归找 `.agents` 的最大深度。
/// 覆盖: computer `user/cid` (2 层) + web 多租户 `tenant/space/project` (3 层) + UserApp `apps/app_id` (2 层)。
const MAX_SCAN_DEPTH: u32 = 4;

/// 启动 skill sync reconciler 后台 task (不阻塞 rcoder 主流程)。
pub fn spawn_skill_sync_reconciler() {
    if !enabled() {
        info!("[SKILL_SYNC] disabled by {ENV_ENABLED}, skip");
        return;
    }
    info!("[SKILL_SYNC] starting skill sync reconciler (background)");
    tokio::spawn(async move {
        if let Err(e) = run_reconcile().await {
            warn!("[SKILL_SYNC] reconciler failed: {e}");
        }
    });
}

fn enabled() -> bool {
    std::env::var(ENV_ENABLED)
        .ok()
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(true)
}

#[derive(Default)]
struct Stats {
    synced: u32,
    skipped: u32,
    failed: u32,
}

async fn run_reconcile() -> Result<(), String> {
    let current = file_server::sync_target_version();
    let mut stats = Stats::default();
    for root in [WORKSPACE_ROOT, COMPUTER_WORKSPACE_ROOT] {
        let root = PathBuf::from(root);
        // 根不存在 (如本环境无 computer workspace) → 跳过, 不算错误
        if tokio::fs::metadata(&root).await.is_err() {
            continue;
        }
        scan_and_reconcile(&root, 0, &current, &mut stats).await;
    }
    info!(
        "[SKILL_SYNC] completed: synced={}, skipped={}, failed={}",
        stats.synced, stats.skipped, stats.failed
    );
    Ok(())
}

/// 递归扫描: 遇含 `.agents` 子目录的目录即当 workspace 处理 (不再下钻); 否则下钻子目录。
/// 超过 `MAX_SCAN_DEPTH` 停止 (防失控 + 覆盖已知 workspace 层级即可)。
async fn scan_and_reconcile(dir: &Path, depth: u32, current: &str, stats: &mut Stats) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let agents = dir.join(".agents");
    if tokio::fs::try_exists(&agents).await.unwrap_or(false) {
        reconcile_workspace(dir, &agents, current, stats).await;
        return;
    }
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            // async 递归需 Box::pin
            Box::pin(scan_and_reconcile(&entry.path(), depth + 1, current, stats)).await;
        }
    }
}

/// 单个 workspace: 读版本 marker 决定是否补 sync。
/// - 无 `.agents/skills` → skip (空 workspace 无需 sync)
/// - `.sync_version` == 当前版本 → skip (O(1), 已同步)
/// - 否则 → `sync_agents` (内部写新 marker)
async fn reconcile_workspace(workspace: &Path, agents: &Path, current: &str, stats: &mut Stats) {
    if !tokio::fs::try_exists(agents.join("skills"))
        .await
        .unwrap_or(false)
    {
        stats.skipped += 1;
        return;
    }
    let marker = agents.join(".sync_version");
    let existing = tokio::fs::read_to_string(&marker).await.unwrap_or_default();
    if existing == current {
        stats.skipped += 1;
        return;
    }
    match file_server::sync_agents(workspace).await {
        Ok(()) => {
            stats.synced += 1;
            info!(
                "[SKILL_SYNC] synced {} ({} -> {})",
                workspace.display(),
                if existing.is_empty() {
                    "(none)"
                } else {
                    existing.as_str()
                },
                current,
            );
        }
        Err(e) => {
            stats.failed += 1;
            warn!("[SKILL_SYNC] sync {} failed: {e}", workspace.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconcile_syncs_outdated_workspace_and_writes_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // 构造 workspace: .../user1/cid1/.agents/skills/sk1/SKILL.md (无 marker → 版本落后)
        let ws = tmp.path().join("user1").join("cid1");
        tokio::fs::create_dir_all(ws.join(".agents/skills/sk1"))
            .await
            .unwrap();
        tokio::fs::write(ws.join(".agents/skills/sk1/SKILL.md"), "x")
            .await
            .unwrap();

        let current = file_server::sync_target_version();
        let mut stats = Stats::default();
        scan_and_reconcile(tmp.path(), 0, &current, &mut stats).await;

        assert_eq!(stats.synced, 1);
        assert_eq!(stats.skipped, 0);
        // sync_agents 写了 marker + fan-out 到 6 家 (验证 grok/pi 补齐)
        assert_eq!(
            tokio::fs::read_to_string(ws.join(".agents/.sync_version"))
                .await
                .unwrap(),
            current
        );
        assert!(ws.join(".claude/skills/sk1/SKILL.md").exists());
        assert!(ws.join(".grok/skills/sk1/SKILL.md").exists());
        assert!(ws.join(".pi/skills/sk1/SKILL.md").exists());
    }

    #[tokio::test]
    async fn reconcile_skips_uptodate_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        tokio::fs::create_dir_all(ws.join(".agents/skills"))
            .await
            .unwrap();
        let current = file_server::sync_target_version();
        // 预置当前版本 marker → 应 skip (不动文件)
        tokio::fs::write(ws.join(".agents/.sync_version"), &current)
            .await
            .unwrap();

        let mut stats = Stats::default();
        scan_and_reconcile(tmp.path(), 0, &current, &mut stats).await;

        assert_eq!(stats.synced, 0);
        assert_eq!(stats.skipped, 1);
        // skip 时不应创建镜像目录
        assert!(!ws.join(".claude").exists());
    }

    #[tokio::test]
    async fn reconcile_skips_agents_without_skills() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        tokio::fs::create_dir_all(ws.join(".agents")).await.unwrap(); // .agents 无 skills 子目录

        let current = file_server::sync_target_version();
        let mut stats = Stats::default();
        scan_and_reconcile(tmp.path(), 0, &current, &mut stats).await;

        assert_eq!(stats.synced, 0);
        assert_eq!(stats.skipped, 1);
    }

    #[tokio::test]
    async fn scan_respects_max_depth() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // 构造 depth 5 的 .agents (a/b/c/d/e/.agents, e 在 depth 5 > MAX_SCAN_DEPTH 4) → 不处理
        let deep = tmp.path().join("a/b/c/d/e");
        tokio::fs::create_dir_all(deep.join(".agents/skills"))
            .await
            .unwrap();

        let mut stats = Stats::default();
        scan_and_reconcile(tmp.path(), 0, "unused", &mut stats).await;

        // 超深度 → 全不处理
        assert_eq!(stats.synced + stats.skipped + stats.failed, 0);
    }
}

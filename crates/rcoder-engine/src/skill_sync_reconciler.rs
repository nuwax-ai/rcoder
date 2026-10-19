//! 启动时后台 reconciler: 对 fan-out 与权威源漂移的 workspace 自动补 sync_agents。
//!
//! 解决: fan-out 列表扩展 (加 grok/pi/cursor...) 后, 已存在的旧 workspace 缺新 agent 目录,
//! 以及源后续变化 (push 新技能) 后实体副本陈旧, 靠手动 push-skills 逐个补齐不可行。
//! 本模块启动时后台遍历所有 workspace, 以「源 vs 各实体目标顶层名称集比对」驱动
//! ( [`file_server::fanout_names_drifted`] ), 一致则跳过; 漂移才补 sync。
//! 历史版本 marker (`.agents/.sync_version`) 只记目标名集合、不反映内容漂移,
//! 不再作为跳过判据 (marker 本身仍由 sync_agents 成功后写入, 兼容观测)。
//!
//! sync 持 per-workspace OS 锁 (`file_server` 内部), 多 Main 副本共享存储上串行;
//! 锁被其他副本占用时本轮跳过 (busy ≠ 失败)。
//! env `RCODER_SKILL_SYNC_RECONCILE_ON_STARTUP` 控制 (默认 true, 因轻量)。

use std::path::{Path, PathBuf};

use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};
use tracing::{info, warn};

/// env 开关名 (默认 true: 名称集比对轻量, 日常启动几乎零开销)。
const ENV_ENABLED: &str = "RCODER_SKILL_SYNC_RECONCILE_ON_STARTUP";

/// reconciler 的工作区锁等待窗口: 短等待, busy 即跳过 (另一 Main 副本正在同步
/// 同一工作区, 无需重复执行)。测试态用更短窗口避免用例空转。
#[cfg(test)]
const RECONCILE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
#[cfg(not(test))]
const RECONCILE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// 递归找 `.agents` 的最大深度。
/// 覆盖: computer `user/cid` (2 层) + web 多租户 `tenant/space/project` (3 层) + Userapp `apps/app_id` (2 层)。
const MAX_SCAN_DEPTH: u32 = 4;

/// 启动 skill sync reconciler 后台 task (不阻塞 rcoder 主流程)。
pub fn spawn_skill_sync_reconciler(
    stop: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<Result<(), String>>> {
    if !enabled() {
        info!("[SKILL_SYNC] disabled by {ENV_ENABLED}, skip");
        return None;
    }
    info!("[SKILL_SYNC] starting skill sync reconciler (background)");
    Some(tokio::spawn(async move {
        let result = run_reconcile(&stop).await;
        if let Err(error) = &result {
            warn!(%error, "Skill sync failed");
        }
        result
    }))
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

async fn run_reconcile(stop: &tokio_util::sync::CancellationToken) -> Result<(), String> {
    let current = file_server::sync_target_version();
    let mut stats = Stats::default();
    for root in [WORKSPACE_ROOT, COMPUTER_WORKSPACE_ROOT] {
        if stop.is_cancelled() {
            break;
        }
        let root = PathBuf::from(root);
        // 根不存在 (如本环境无 computer workspace) → 跳过, 不算错误
        if tokio::fs::metadata(&root).await.is_err() {
            continue;
        }
        scan_and_reconcile(&root, 0, &current, &mut stats, stop).await;
    }
    info!(
        "[SKILL_SYNC] completed: synced={}, skipped={}, failed={}",
        stats.synced, stats.skipped, stats.failed
    );
    if stats.failed > 0 {
        return Err(format!(
            "Skill sync failed for {} workspace(s)",
            stats.failed
        ));
    }
    Ok(())
}

/// 递归扫描: 遇含 `.agents` 子目录的目录即当 workspace 处理 (不再下钻); 否则下钻子目录。
/// 超过 `MAX_SCAN_DEPTH` 停止 (防失控 + 覆盖已知 workspace 层级即可)。
async fn scan_and_reconcile(
    dir: &Path,
    depth: u32,
    current: &str,
    stats: &mut Stats,
    stop: &tokio_util::sync::CancellationToken,
) {
    if stop.is_cancelled() || depth > MAX_SCAN_DEPTH {
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
        if stop.is_cancelled() {
            break;
        }
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            // async 递归需 Box::pin
            Box::pin(scan_and_reconcile(
                &entry.path(),
                depth + 1,
                current,
                stats,
                stop,
            ))
            .await;
        }
    }
}

/// 单个 workspace: 名称集比对决定是否补 sync (取代版本 marker 等值比较)。
/// - 无 `.agents/skills` → skip (空 workspace 无需 sync)
/// - `.agents/skills` 为目录链接 (agent-store/视图托管布局) → skip
/// - 源与各实体目标顶层名称集一致 → skip
/// - 否则 → `sync_agents_with_wait` (持工作区锁 fan-out, 内部写新 marker);
///   锁 busy → skip (另一副本在同步, 不算失败), 真实错误才计 failed
async fn reconcile_workspace(workspace: &Path, agents: &Path, current: &str, stats: &mut Stats) {
    if !tokio::fs::try_exists(agents.join("skills"))
        .await
        .unwrap_or(false)
    {
        stats.skipped += 1;
        return;
    }
    match file_server::fanout_names_drifted(workspace).await {
        Ok(false) => {
            stats.skipped += 1;
            return;
        }
        Ok(true) => {}
        Err(e) => {
            stats.failed += 1;
            warn!(
                "[SKILL_SYNC] drift check {} failed: {e}",
                workspace.display()
            );
            return;
        }
    }
    match file_server::sync_agents_with_wait(workspace, RECONCILE_LOCK_WAIT).await {
        Ok(file_server::SyncAgentsOutcome::Completed) => {
            stats.synced += 1;
            info!(
                "[SKILL_SYNC] synced {} (fanout names -> {})",
                workspace.display(),
                current,
            );
        }
        Ok(file_server::SyncAgentsOutcome::BusySkipped) => {
            stats.skipped += 1;
            info!(
                "[SKILL_SYNC] workspace busy, skipped this pass: {}",
                workspace.display()
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
    async fn skill_shutdown_before_scan_leaves_workspace_and_marker_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("app");
        tokio::fs::create_dir_all(workspace.join(".agents/skills/one"))
            .await
            .unwrap();
        tokio::fs::write(workspace.join(".agents/skills/one/SKILL.md"), "original")
            .await
            .unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            &file_server::sync_target_version(),
            &mut stats,
            &stop,
        )
        .await;
        assert_eq!(stats.synced + stats.skipped + stats.failed, 0);
        assert!(!workspace.join(".agents/.sync_version").exists());
        assert!(!workspace.join(".claude").exists());
        assert_eq!(
            tokio::fs::read_to_string(workspace.join(".agents/skills/one/SKILL.md"))
                .await
                .unwrap(),
            "original"
        );
    }

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
        scan_and_reconcile(
            tmp.path(),
            0,
            &current,
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(stats.synced, 1);
        assert_eq!(stats.skipped, 0);
        // sync_agents 写了 marker + fan-out 到活跃目标 (grok/pi 临时屏蔽不再创建)
        assert_eq!(
            tokio::fs::read_to_string(ws.join(".agents/.sync_version"))
                .await
                .unwrap(),
            current
        );
        assert!(ws.join(".claude/skills/sk1/SKILL.md").exists());
        assert!(!ws.join(".grok/skills").exists());
        assert!(!ws.join(".pi/skills").exists());
    }

    #[tokio::test]
    async fn reconcile_skips_when_target_name_sets_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        // 源与全部实体目标顶层名称集一致 → skip (marker 内容无关, 不做任何写)
        for dir in [".agents", ".claude", ".opencode", ".codex"] {
            tokio::fs::create_dir_all(ws.join(dir).join("skills/sk1"))
                .await
                .unwrap();
            tokio::fs::create_dir_all(ws.join(dir).join("agents"))
                .await
                .unwrap();
        }
        let current = file_server::sync_target_version();
        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            &current,
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(stats.synced, 0);
        assert_eq!(stats.skipped, 1);
        // skip 时不应写 marker 或重写目录
        assert!(!ws.join(".agents/.sync_version").exists());
    }

    #[tokio::test]
    async fn reconcile_heals_drifted_name_set_despite_current_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        for name in ["sk1", "sk2"] {
            tokio::fs::create_dir_all(ws.join(".agents/skills").join(name))
                .await
                .unwrap();
            tokio::fs::write(ws.join(".agents/skills").join(name).join("SKILL.md"), name)
                .await
                .unwrap();
        }
        // 实体副本只有 sk1 (生产 15/13 漂移的最小形态), 且 marker 预置为"最新"——
        // 修复前 marker 等值比较会跳过、漂移永不自愈
        for dir in [".claude", ".opencode", ".codex"] {
            tokio::fs::create_dir_all(ws.join(dir).join("skills/sk1"))
                .await
                .unwrap();
        }
        tokio::fs::write(
            ws.join(".agents/.sync_version"),
            file_server::sync_target_version(),
        )
        .await
        .unwrap();

        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            &file_server::sync_target_version(),
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(stats.synced, 1, "stale marker must not block self-healing");
        for dir in [".claude", ".opencode", ".codex"] {
            assert!(
                ws.join(dir).join("skills/sk2/SKILL.md").exists(),
                "{dir} healed to source name set"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_skips_link_managed_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = tmp.path().join("u1");
        let ws = user.join("c1");
        let store = user.join(".agent-store").join("44").join("skills");
        tokio::fs::create_dir_all(&store).await.unwrap();
        tokio::fs::create_dir_all(ws.join(".agents")).await.unwrap();
        // `.agents/skills` 为目录链 (agent-store 布局): legacy fan-out 不得介入
        std::os::unix::fs::symlink("../../.agent-store/44/skills", ws.join(".agents/skills"))
            .unwrap();

        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            "unused",
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.synced + stats.failed, 0);
        assert!(
            !ws.join(".claude").exists(),
            "link-managed layout must not be materialized"
        );
    }

    #[tokio::test]
    async fn reconcile_counts_busy_as_skipped_not_failed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        tokio::fs::create_dir_all(ws.join(".agents/skills/sk1"))
            .await
            .unwrap();
        // 原始 flock 占用工作区锁 (模拟另一 Main 副本正在同步同一工作区)
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(ws.join(".agents/.skill-preservation.lock"))
            .unwrap();
        lock.try_lock().unwrap();

        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            "unused",
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(stats.synced, 0);
        assert_eq!(stats.failed, 0, "busy is not a failure");
        assert_eq!(stats.skipped, 1);
        // busy 不写 marker、不动目标
        assert!(!ws.join(".agents/.sync_version").exists());
        assert!(!ws.join(".claude").exists());
    }

    #[tokio::test]
    async fn reconcile_skips_agents_without_skills() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        tokio::fs::create_dir_all(ws.join(".agents")).await.unwrap(); // .agents 无 skills 子目录

        let current = file_server::sync_target_version();
        let mut stats = Stats::default();
        scan_and_reconcile(
            tmp.path(),
            0,
            &current,
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

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
        scan_and_reconcile(
            tmp.path(),
            0,
            "unused",
            &mut stats,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        // 超深度 → 全不处理
        assert_eq!(stats.synced + stats.skipped + stats.failed, 0);
    }
}

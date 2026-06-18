//! VNC 活跃时间同步任务
//!
//! 每 30s 扫描 pingora 的 `vnc_activity`（user_id → last_seen Unix 秒），
//! 对近期有 VNC 流量的 user_id 找到对应 project_id 并调 `update_activity`，
//! 防止 cleanup_task 在用户使用 VNC 桌面期间误判 idle 并销毁容器。
//!
//! ## 与其他活跃信号的关系
//!
//! | 信号 | 是否更新活跃时间 |
//! |---|---|
//! | chat 入口（用户主动） | ✅ |
//! | agent 任务进度 SSE（非心跳） | ✅ Bug 5 修复 |
//! | SSE 心跳 | ❌ |
//! | status_checker 后台确认 active_tasks | ✅ |
//! | **VNC 远程桌面使用** | ✅ **本任务** |
//!
//! ## 工作流程
//!
//! ```text
//! [pingora] VNC WS chunk 触发
//!     ↓ record_vnc_activity 节流 10s 写入
//! [vnc_activity: DashMap<user_id, AtomicI64>]
//!     ↓ 每 30s 扫描
//! [本任务 sync_once]
//!     ↓ find_projects_by_user_id
//!     ↓ update_activity
//! [storage: project.last_activity + container.last_activity]
//!     ↓ cleanup_task 看到"近期活跃"
//! [容器不被销毁]
//! ```
//!
//! 注意：本模块由 binary (main.rs) 通过 background_tasks 使用，lib 内不直接调用。

#![allow(dead_code)]

use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

use crate::router::AppState;

/// VNC 活跃时间同步配置
#[derive(Debug, Clone)]
pub struct VncActivitySyncConfig {
    /// 同步间隔（默认 30s）
    pub sync_interval: Duration,
    /// VNC 活跃判定窗口：last_seen 在此窗口内才算"近期活跃"（默认 60s）
    /// 略大于 sync_interval，避免边界 race（如刚好 30s 没新 chunk 但用户还在用）
    pub active_window: Duration,
}

impl Default for VncActivitySyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(30),
            active_window: Duration::from_secs(60),
        }
    }
}

/// 启动 VNC 活跃时间同步后台任务
///
/// 任务会在每次 tick 时执行 `sync_once`：扫描 pingora 的 vnc_activity 快照，
/// 对近期活跃的 user 续期 storage 活跃时间，并清理过期的 vnc_activity 条目。
pub fn start_vnc_activity_sync_task(
    state: Arc<AppState>,
    config: VncActivitySyncConfig,
) -> tokio::task::JoinHandle<()> {
    info!(
        "🔄 [VNC_ACTIVITY_SYNC] Starting: interval={}s, active_window={}s",
        config.sync_interval.as_secs(),
        config.active_window.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);
        // Skip 模式：避免累积 tick 在系统繁忙时连发
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            sync_once(&state, &config).await;
        }
    })
}

/// 执行一次同步
///
/// 步骤：
/// 1. 拿 pingora vnc_activity 快照（释放 DashMap 读锁后才操作 storage）
/// 2. 遍历快照，过滤近期活跃的 user
/// 3. 通过 user_id 找关联 project_id（可能多个，多 project 共享容器场景）
/// 4. 对每个 project 调 update_activity（同时刷新 project + container 活跃时间）
/// 5. 清理 vnc_activity 中过期条目（避免无限增长）
async fn sync_once(state: &Arc<AppState>, config: &VncActivitySyncConfig) {
    let Some(ref pingora) = state.pingora_service else {
        // pingora 未启用（如纯 CLI 模式），跳过
        return;
    };

    let now = Utc::now().timestamp();
    let window_secs = config.active_window.as_secs() as i64;

    // 拿快照（独立函数，便于测试）
    let stats = process_vnc_activity_snapshot(state, pingora, now, window_secs).await;

    // 清理过期条目（2 倍窗口后清理，避免边界 race）
    let evicted = pingora.evict_stale_vnc_activity(window_secs * 2);

    debug!(
        "✅ [VNC_ACTIVITY_SYNC] Processed {} active user(s), updated {} project(s), evicted {} stale entr(ies)",
        stats.active_users, stats.updated_projects, evicted
    );
}

/// 一次同步的统计（便于日志）
#[derive(Debug, Default, Clone, Copy)]
struct SyncStats {
    /// 近期活跃的 user 数
    active_users: usize,
    /// 实际更新活跃时间的 project 数
    updated_projects: usize,
}

/// 处理 vnc_activity 快照，对所有近期活跃的 user 找到关联 project 并续期
async fn process_vnc_activity_snapshot(
    state: &Arc<AppState>,
    pingora: &rcoder_proxy::PingoraProxyService,
    now_secs: i64,
    window_secs: i64,
) -> SyncStats {
    let snapshot = pingora.vnc_activity_snapshot();
    let mut stats = SyncStats::default();

    for (user_id, last_seen) in snapshot {
        if now_secs - last_seen > window_secs {
            // 超窗口，跳过（已被或将被 evict_stale_vnc_activity 清理）
            continue;
        }
        stats.active_users += 1;

        // 一个 user_id 可能关联多个 project（多 project 共享容器场景）
        // find_projects_by_user_id 内部是 O(N) 全量遍历，但 N 通常很小
        let projects = state.projects.find_projects_by_user_id(&user_id);
        for project in projects {
            let pid = project.project_id().to_string();
            if state.update_activity(&pid).is_some() {
                stats.updated_projects += 1;
            } else {
                // project 在快照之后被并发 remove（如 cleanup_task 销毁）—— 正常情况，不 warn
                debug!(
                    "[VNC_ACTIVITY_SYNC] update_activity returned None: project_id={} may have been concurrently removed",
                    pid
                );
            }
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values_are_reasonable() {
        let config = VncActivitySyncConfig::default();
        // sync_interval 应该远小于 cleanup_task 的 idle_timeout（默认 30min）
        assert!(config.sync_interval.as_secs() < 1800);
        // active_window 略大于 sync_interval
        assert!(config.active_window.as_secs() >= config.sync_interval.as_secs());
    }

    #[test]
    fn test_sync_stats_default() {
        let stats = SyncStats::default();
        assert_eq!(stats.active_users, 0);
        assert_eq!(stats.updated_projects, 0);
    }
}

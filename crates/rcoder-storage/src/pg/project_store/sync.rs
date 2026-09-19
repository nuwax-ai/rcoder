//! 跨副本镜像同步：周期全量 diff（P2-M1）
//!
//! 多副本下各副本镜像独立，本任务让副本看到其他副本的提交：
//! 1. **排空屏障**：wait_drained 等本副本在途 op 全部落库——此后读到的 PG 快照
//!    必然包含本副本全部已提交写；
//! 2. **全量 diff**：拉三表快照，与镜像比对——PG 有而镜像无 → 补入（hydrate），
//!    镜像有而 PG 无 → 移除（屏障保证不是本副本未落库的写，而是远端删除），
//!    两边都有 → 逐字段签名比对，变更才重建。
//! 3. 应用一律走 `inner.*`（内存实现）——**旁路持久化**（数据本就来自 PG，
//!    回写既是空转又可能与本副本写竞序）。
//!
//! 数据量 = 活跃 project 数（百级），全量 diff 开销可忽略；若未来量级增长，
//! 再演进为 updated_at 水位增量 + 墓碑删除。
//!
//! 陈旧窗口 = 同步周期（默认 5s）：ClientIP affinity 下常规流量无感知；
//! 仅副本故障切换后的首个请求可能读到 ≤5s 陈旧数据（客户端重连即恢复）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::db::owner::DatabaseOwner;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::load::{container_rows_to_map, hydrate_project};
use super::repo;
use crate::pg::PgStore;

/// 同步周期（ClientIP affinity 下常规流量不受影响；故障切换陈旧窗口上限）
const SYNC_INTERVAL: Duration = Duration::from_secs(5);
/// 排空屏障上限（writer 常规毫秒级；超时跳过本轮，下轮再试）
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 同步循环（PG 模式由 rcoder background_tasks 拉起）。
///
/// 持有 `Arc<PgStore>` 独立句柄（从 `ProjectStoreBackend::postgres()` clone）——
/// pg 子树不依赖 crate 根门面，依赖图保持单向（backend → pg）。
pub async fn run_sync_loop(store: Arc<PgStore>, mut shutdown_rx: broadcast::Receiver<()>) {
    info!("[STORAGE_PG] cross-replica sync started (interval={SYNC_INTERVAL:?})");
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => break,
            _ = tokio::time::sleep(SYNC_INTERVAL) => {
                if let Err(e) = sync_once(&store, store.inner(), &store.database).await {
                    warn!("[STORAGE_PG] cross-replica sync failed (will retry): {e:#}");
                }
            }
        }
    }
    info!("[STORAGE_PG] cross-replica sync stopped");
}

/// 单轮同步（pub(crate) 供集成测试直接驱动）
pub(crate) async fn sync_once(
    store: &PgStore,
    inner: &crate::adapter::ProjectAdapter,
    pool: &DatabaseOwner,
) -> anyhow::Result<()> {
    // 排空屏障：本副本写全部落库后，PG 快照才可作为 diff 基准。
    // 超时（PG 故障重试中）跳过本轮，避免用旧快照误删镜像条目。
    if !store.wait_drained(DRAIN_TIMEOUT).await {
        debug!("[STORAGE_PG] sync skipped: drain barrier timeout (writer backlogged?)");
        return Ok(());
    }

    let (baseline, baseline_containers): (HashMap<_, _>, _) = {
        let _registration = store.registration.lock().unwrap_or_else(|p| p.into_inner());
        (
            inner.iter().into_iter().collect(),
            store
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        )
    };

    // 三表读入单事务（REPEATABLE READ）：三次独立 pool acquire 会拿到三个
    // 不同语句快照，跨快照可产生瞬态"孤儿 session"（project 在快照 1 有、
    // 快照 2 无）触发误判。单事务保证三表同一一致性视图。
    let (containers, projects, sessions) = super::database::snapshot(pool).await?;

    // PG 侧索引
    let pg_project_ids: std::collections::HashSet<&str> =
        projects.iter().map(|p| p.project_id.as_str()).collect();
    let pg_session_ids: std::collections::HashSet<&str> =
        sessions.iter().map(|s| s.session_id.as_str()).collect();

    // 1) 删除：镜像有而 PG 无。排空屏障只覆盖屏障前的写——快照 fetch 之后
    //    本地新增（内存有、op 尚在队列）会被误判为"远端删除"。对每个候选
    //    做单行二次确认（短排空 + 直查）：屏障后新写若已落库则直查命中。
    let mirror_projects: Vec<String> = inner.iter().into_iter().map(|(pid, _)| pid).collect();
    for project_id in mirror_projects {
        if !pg_project_ids.contains(project_id.as_str())
            && !self_confirm_project_alive(store, pool, &project_id).await
        {
            debug!("[STORAGE_PG] sync remove project {project_id} (deleted on peer replica)");
            let _registration = store.registration.lock().unwrap_or_else(|p| p.into_inner());
            if let (Some(expected), Some(current)) =
                (baseline.get(&project_id), inner.get(&project_id))
                && Arc::ptr_eq(expected, &current)
            {
                inner.remove(&project_id);
            }
        }
    }
    // session 删除：镜像各 project 的 session 中不在 PG 的（同样二次确认）
    let mirror_sessions: Vec<(String, String)> = inner
        .iter()
        .into_iter()
        .flat_map(|(pid, info)| {
            info.sessions()
                .iter()
                .map(|sid| (pid.clone(), sid.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    for (project_id, sid) in mirror_sessions {
        if !pg_session_ids.contains(sid.as_str())
            && !self_confirm_session_alive(store, pool, &sid).await
        {
            debug!("[STORAGE_PG] sync remove session {sid} (deleted on peer replica)");
            let _registration = store.registration.lock().unwrap_or_else(|p| p.into_inner());
            if let (Some(expected), Some(current)) =
                (baseline.get(&project_id), inner.get(&project_id))
                && expected.persistence_identity().generation
                    == current.persistence_identity().generation
                && expected.persistence_identity().sessions.get(&sid)
                    == current.persistence_identity().sessions.get(&sid)
            {
                inner.clear_session_one(&project_id, &sid);
            }
        }
    }

    // 2) 新增/变更：PG 有而镜像无 → 补入；都有 → 签名比对后按需重建
    let container_by_name = container_rows_to_map(containers);
    let mut changed = 0usize;
    let mut added = 0usize;
    for row in projects {
        let _registration = store.registration.lock().unwrap_or_else(|p| p.into_inner());
        let existing = inner.get(&row.project_id);
        if let Some(current) = &existing {
            if baseline
                .get(&row.project_id)
                .is_none_or(|previous| !Arc::ptr_eq(previous, current))
            {
                continue; // Local mutation after snapshot admission wins until the next sync.
            }
        } else if baseline.contains_key(&row.project_id) {
            continue;
        }
        let info = hydrate_project(&row, &container_by_name)?;
        // A peer replacement must update registration and hydrated project
        // together. A local container intent after snapshot admission prevents
        // applying the entire associated project, not just its registry entry.
        let registration_update = if let Some(name) = &row.container_name {
            let container = container_by_name
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Project references missing container snapshot"))?;
            let target = container.persistence_identity();
            let registrations = store
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let current = registrations.get(name);
            if current != baseline_containers.get(name) && current != Some(&target) {
                continue;
            }
            // Equality with target permits another project in this same snapshot
            // to hydrate after the first project already refreshed this registry.
            Some((name.clone(), target))
        } else {
            None
        };
        inner.insert(row.project_id.clone(), Arc::new(info))?;
        // Publish only after mirror insertion succeeds. The outer registration
        // guard still excludes local writes and other snapshot application.
        if let Some((name, target)) = registration_update {
            store
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(name, target);
        }
        if existing.is_some() {
            changed += 1;
        } else {
            added += 1;
        }
    }

    if added + changed > 0 {
        info!("[STORAGE_PG] cross-replica sync applied: +{added} projects, {changed} changed");
    }
    Ok(())
}

/// remove 候选的二次确认：短排空后直查该行是否已落库（屏障后新写防误删）。
/// PG 故障时返回 true（宁可不删也不误删——下轮 sync 再收敛）。
async fn self_confirm_project_alive(
    store: &PgStore,
    pool: &DatabaseOwner,
    project_id: &str,
) -> bool {
    if store.wait_drained(Duration::from_millis(200)).await {
        let id = project_id.to_owned();
        match super::database::read(pool, move |tx| {
            Box::pin(async move { repo::project_exists(tx, &id).await })
        })
        .await
        {
            Ok(alive) => alive,
            Err(e) => {
                debug!("[STORAGE_PG] sync remove-confirm query failed (keeping): {e}");
                true
            }
        }
    } else {
        true // 排空超时（writer 积压）：偏保守不删
    }
}

/// session 版二次确认（sessions 表 PK 直查）
async fn self_confirm_session_alive(
    store: &PgStore,
    pool: &DatabaseOwner,
    session_id: &str,
) -> bool {
    if store.wait_drained(Duration::from_millis(200)).await {
        let id = session_id.to_owned();
        match super::database::read(pool, move |tx| {
            Box::pin(async move { repo::session_exists(tx, &id).await })
        })
        .await
        {
            Ok(alive) => alive,
            Err(e) => {
                debug!("[STORAGE_PG] sync remove-confirm query failed (keeping): {e}");
                true
            }
        }
    } else {
        true
    }
}

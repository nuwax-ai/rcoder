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

use sqlx::PgPool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::load::{container_rows_to_map, hydrate_project};
use super::repo;
use crate::backend::ProjectStoreBackend;

/// 同步周期（ClientIP affinity 下常规流量不受影响；故障切换陈旧窗口上限）
const SYNC_INTERVAL: Duration = Duration::from_secs(5);
/// 排空屏障上限（writer 常规毫秒级；超时跳过本轮，下轮再试）
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 同步循环（PG 模式由 rcoder background_tasks 拉起）。
///
/// 持有 `Arc<ProjectStoreBackend>` 每 tick 经 `postgres()` 取借用（借用跨 await
/// 合法：Arc 在任务内存活），避免 PgStore 自持 Arc 循环。
pub async fn run_sync_loop(
    projects: Arc<ProjectStoreBackend>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    info!("[STORAGE_PG] cross-replica sync started (interval={SYNC_INTERVAL:?})");
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => break,
            _ = tokio::time::sleep(SYNC_INTERVAL) => {
                let Some(store) = projects.postgres() else {
                    break; // 后端不可能中途切走（构造期决定），防御性退出
                };
                if let Err(e) = sync_once(store, store.inner(), store.pool()).await {
                    warn!("[STORAGE_PG] cross-replica sync failed (will retry): {e:#}");
                }
            }
        }
    }
    info!("[STORAGE_PG] cross-replica sync stopped");
}

/// 单轮同步（pub(crate) 供集成测试直接驱动）
pub(crate) async fn sync_once(
    store: &crate::pg::PgStore,
    inner: &crate::adapter::ProjectAdapter,
    pool: &PgPool,
) -> anyhow::Result<()> {
    // 排空屏障：本副本写全部落库后，PG 快照才可作为 diff 基准。
    // 超时（PG 故障重试中）跳过本轮，避免用旧快照误删镜像条目。
    if !store.wait_drained(DRAIN_TIMEOUT).await {
        debug!("[STORAGE_PG] sync skipped: drain barrier timeout (writer backlogged?)");
        return Ok(());
    }

    let containers = repo::fetch_all_containers(pool).await?;
    let projects = repo::fetch_all_projects(pool).await?;
    let sessions = repo::fetch_all_sessions(pool).await?;

    // PG 侧索引
    let pg_project_ids: std::collections::HashSet<&str> =
        projects.iter().map(|p| p.project_id.as_str()).collect();
    let pg_session_ids: std::collections::HashSet<&str> =
        sessions.iter().map(|s| s.session_id.as_str()).collect();

    // 1) 删除：镜像有而 PG 无（屏障保证 = 远端删除）
    let mirror_projects = inner.iter();
    for (project_id, _) in mirror_projects {
        if !pg_project_ids.contains(project_id.as_str()) {
            debug!("[STORAGE_PG] sync remove project {project_id} (deleted on peer replica)");
            inner.remove(&project_id);
        }
    }
    // session 删除：镜像各 project 的 session 中不在 PG 的（经 inner 反查逐个清）
    for (project_id, info) in inner.iter() {
        for sid in info.sessions() {
            if !pg_session_ids.contains(sid.as_str()) {
                debug!("[STORAGE_PG] sync remove session {sid} (deleted on peer replica)");
                inner.clear_session_one(&project_id, &sid);
            }
        }
    }

    // 2) 新增/变更：PG 有而镜像无 → 补入；都有 → 签名比对后按需重建
    let container_by_name = container_rows_to_map(containers);
    let mut changed = 0usize;
    let mut added = 0usize;
    let mut sessions_by_project: HashMap<String, Vec<String>> = HashMap::new();
    for row in &sessions {
        sessions_by_project
            .entry(row.project_id.clone())
            .or_default()
            .push(row.session_id.clone());
    }
    for row in projects {
        let existing = inner.get(&row.project_id);
        if let Some(current) = &existing
            && project_signature(current) == row_signature(&row)
        {
            continue; // 无变化
        }
        let Some(info) = hydrate_project(&row, &container_by_name) else {
            continue; // hydrate 内已告警（service_type 未知等）
        };
        if let Err(e) = inner.insert(row.project_id.clone(), Arc::new(info)) {
            warn!(
                "[STORAGE_PG] sync insert failed for {}: {e:#}（跳过）",
                row.project_id
            );
            continue;
        }
        if existing.is_some() {
            changed += 1;
        } else {
            added += 1;
        }
    }

    // 3) session 补入：PG 有而镜像无（project 刚补入时其 session 由 add 补齐；
    //    project 已存在的新 session 单独补）
    let mut added_sessions = 0usize;
    for row in &sessions {
        let Some(info) = inner.get(&row.project_id) else {
            continue; // 孤儿 session（project 行缺失，FK 下不应出现）
        };
        if !info.sessions().contains(row.session_id.as_str()) {
            inner.add_session_to_project(&row.project_id, &row.session_id);
            added_sessions += 1;
        }
    }

    if added + changed + added_sessions > 0 {
        info!(
            "[STORAGE_PG] cross-replica sync applied: +{added} projects, {changed} changed, +{added_sessions} sessions"
        );
    }
    Ok(())
}

/// 镜像侧轻量签名（避开全字段深比较；volatile 的 last_activity 不参与，
/// 容器以 container_name 对齐——行侧 container_name 即容器表键）
type ProjectSig = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn project_signature(info: &shared_types::ProjectAndContainerInfo) -> ProjectSig {
    (
        info.user_id().map(str::to_string),
        info.pod_id().map(str::to_string),
        info.container_info().map(|c| c.container_name),
        info.latest_session().map(str::to_string),
        info.request_id().map(str::to_string),
        info.model_provider().map(|p| p.id.clone()),
    )
}

fn row_signature(row: &repo::ProjectRow) -> ProjectSig {
    (
        row.user_id.clone(),
        row.pod_id.clone(),
        row.container_name.clone(),
        row.latest_session.clone(),
        row.request_id.clone(),
        row.model_provider
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    )
}

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

use super::PgStore;
use super::load::{container_rows_to_map, hydrate_project};
use super::repo;

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
                if let Err(e) = sync_once(&store, store.inner(), store.pool()).await {
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
    pool: &PgPool,
) -> anyhow::Result<()> {
    // 排空屏障：本副本写全部落库后，PG 快照才可作为 diff 基准。
    // 超时（PG 故障重试中）跳过本轮，避免用旧快照误删镜像条目。
    if !store.wait_drained(DRAIN_TIMEOUT).await {
        debug!("[STORAGE_PG] sync skipped: drain barrier timeout (writer backlogged?)");
        return Ok(());
    }

    // 三表读入单事务（REPEATABLE READ）：三次独立 pool acquire 会拿到三个
    // 不同语句快照，跨快照可产生瞬态"孤儿 session"（project 在快照 1 有、
    // 快照 2 无）触发误判。单事务保证三表同一一致性视图。
    let mut snap_tx = pool
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    let containers = repo::fetch_all_containers(&mut *snap_tx).await?;
    let projects = repo::fetch_all_projects(&mut *snap_tx).await?;
    let sessions = repo::fetch_all_sessions(&mut *snap_tx).await?;
    drop(snap_tx); // 只读快照，即可释放

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
            inner.remove(&project_id);
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
            inner.clear_session_one(&project_id, &sid);
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
        let Some(mut info) = hydrate_project(&row, &container_by_name) else {
            continue; // hydrate 内已告警（service_type 未知等）
        };
        // merge 语义：整条 insert 会把快照后本地新增的 session 抛掉（hydrate
        // 出的 info 无 session 集合）——先保留镜像现有 sessions 再替换
        if let Some(current) = &existing {
            for sid in current.sessions().iter() {
                info.add_session(sid.clone());
            }
        }
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

/// 镜像/行侧统一签名（字段拼接；volatile 的 last_activity/created_at/version 不参与）。
/// 覆盖全部业务可变字段：漏字段 = 远端变更永不感知（镜像长期陈旧）——
/// agent_status（状态同步）、租户三元组、service_type、model_provider 全量 JSON
/// （base_url/api_key 变更不只看 id）都参与比对。
fn project_signature(info: &shared_types::ProjectAndContainerInfo) -> String {
    let mut sig = String::with_capacity(128);
    sig.push_str("u:");
    sig.push_str(info.user_id().unwrap_or(""));
    sig.push_str("|p:");
    sig.push_str(info.pod_id().unwrap_or(""));
    sig.push_str("|c:");
    sig.push_str(
        &info
            .container_info()
            .map(|c| c.container_name)
            .unwrap_or_default(),
    );
    sig.push_str("|s:");
    sig.push_str(
        info.latest_session()
            .map(str::to_string)
            .unwrap_or_default()
            .as_str(),
    );
    sig.push_str("|r:");
    sig.push_str(
        info.request_id()
            .map(str::to_string)
            .unwrap_or_default()
            .as_str(),
    );
    sig.push_str("|a:");
    sig.push_str(&serde_json::to_string(&info.status()).unwrap_or_default());
    sig.push_str("|t:");
    sig.push_str(info.tenant_id().unwrap_or(""));
    sig.push('/');
    sig.push_str(info.space_id().unwrap_or(""));
    sig.push('/');
    sig.push_str(info.isolation_type().unwrap_or(""));
    sig.push_str("|v:");
    sig.push_str(&serde_json::to_string(&info.service_type()).unwrap_or_default());
    sig.push_str("|m:");
    sig.push_str(&serde_json::to_string(&info.model_provider()).unwrap_or_default());
    sig
}

fn row_signature(row: &repo::ProjectRow) -> String {
    let mut sig = String::with_capacity(128);
    sig.push_str("u:");
    sig.push_str(row.user_id.as_deref().unwrap_or(""));
    sig.push_str("|p:");
    sig.push_str(row.pod_id.as_deref().unwrap_or(""));
    sig.push_str("|c:");
    sig.push_str(row.container_name.as_deref().unwrap_or(""));
    sig.push_str("|s:");
    sig.push_str(row.latest_session.as_deref().unwrap_or(""));
    sig.push_str("|r:");
    sig.push_str(row.request_id.as_deref().unwrap_or(""));
    sig.push_str("|a:");
    sig.push_str(
        &row.agent_status
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    sig.push_str("|t:");
    sig.push_str(row.tenant_id.as_deref().unwrap_or(""));
    sig.push('/');
    sig.push_str(row.space_id.as_deref().unwrap_or(""));
    sig.push('/');
    sig.push_str(row.isolation_type.as_deref().unwrap_or(""));
    sig.push_str("|v:");
    sig.push_str(row.service_type.as_deref().unwrap_or(""));
    sig.push_str("|m:");
    sig.push_str(
        &row.model_provider
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    sig
}

/// remove 候选的二次确认：短排空后直查该行是否已落库（屏障后新写防误删）。
/// PG 故障时返回 true（宁可不删也不误删——下轮 sync 再收敛）。
async fn self_confirm_project_alive(store: &PgStore, pool: &PgPool, project_id: &str) -> bool {
    if store.wait_drained(Duration::from_millis(200)).await {
        let sql = "SELECT 1 FROM projects WHERE project_id = $1";
        match sqlx::query_scalar::<_, i32>(sql)
            .bind(project_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => false,
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
async fn self_confirm_session_alive(store: &PgStore, pool: &PgPool, session_id: &str) -> bool {
    if store.wait_drained(Duration::from_millis(200)).await {
        let sql = "SELECT 1 FROM sessions WHERE session_id = $1";
        match sqlx::query_scalar::<_, i32>(sql)
            .bind(session_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                debug!("[STORAGE_PG] sync remove-confirm query failed (keeping): {e}");
                true
            }
        }
    } else {
        true
    }
}

//! 启动全量加载 + 行→领域对象重建（sync 任务复用）
//!
//! 加载顺序 containers → projects → sessions（与 FK 一致）。
//! 重建走 `inner.insert/insert_with_session/add_session_to_project`（内存实现的
//! 唯一一份业务逻辑），天然旁路持久化（PgStore 的 enqueue 只在其包装层）。
//! session 按非 latest 优先、latest 最后的顺序回放，保证 `latest_session` 复原。

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use shared_types::{
    AgentStatus, ContainerBasicInfo, ModelProviderConfig, ProjectAndContainerInfo, ServiceType,
};

use super::repo::{self, ContainerRow, ProjectRow, SessionRow};
use crate::adapter::ProjectAdapter;

/// 全量加载（容器数 = 历史活跃容器，量级小）
pub(crate) async fn load_all(pool: &PgPool, inner: &ProjectAdapter) -> anyhow::Result<()> {
    let containers = repo::fetch_all_containers(pool).await?;
    let container_by_name = container_rows_to_map(containers);
    let projects = repo::fetch_all_projects(pool).await?;
    let sessions = repo::fetch_all_sessions(pool).await?;
    apply_snapshot(inner, container_by_name, projects, sessions);
    info!("[STORAGE_PG] boot load complete");
    Ok(())
}

/// containers 行 → 基本信息映射（hydrate 与 sync 共用）
pub(super) fn container_rows_to_map(
    containers: Vec<ContainerRow>,
) -> HashMap<String, ContainerBasicInfo> {
    containers
        .into_iter()
        .map(|row| (row.container_name.clone(), container_row_to_basic(&row)))
        .collect()
}

/// 单行转换（回源直查的单容器 hydrate 复用）
pub(super) fn container_row_to_basic(row: &ContainerRow) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: row.container_id.clone().unwrap_or_default(),
        container_name: row.container_name.clone(),
        container_ip: row.container_ip.clone(),
        internal_port: u16::try_from(row.internal_port).unwrap_or(0),
        external_port: u16::try_from(row.external_port).unwrap_or(0),
        project_id: row.logical_id.clone(),
        status: row.status.clone(),
        created_at: row.created_at,
        service_url: row.service_url.clone(),
    }
}

/// project 行 → 领域对象（字段级重建；解码失败 warn 并跳过该字段）
pub(super) fn hydrate_project(
    row: &ProjectRow,
    container_by_name: &HashMap<String, ContainerBasicInfo>,
) -> Option<ProjectAndContainerInfo> {
    // service_type 解析失败（枚举演进）→ 跳过该行并告警（fail safe）
    let service_type = parse_service_type(row.service_type.as_deref()).or_else(|| {
        warn!(
            "[STORAGE_PG] skip project {} with unknown service_type {:?}",
            row.project_id, row.service_type
        );
        None
    })?;
    let mut info = ProjectAndContainerInfo::new(row.project_id.clone());
    info.set_service_type(Some(service_type));
    info.set_user_id(row.user_id.clone());
    info.set_pod_id(row.pod_id.clone());
    info.set_scope(
        row.tenant_id.clone(),
        row.space_id.clone(),
        row.isolation_type.clone(),
    );
    info.set_request_id(row.request_id.clone());
    info.set_timestamps(row.created_at, row.last_activity);
    if let Some(value) = row.model_provider.clone() {
        match serde_json::from_value::<ModelProviderConfig>(value) {
            Ok(provider) => info.set_model_provider(Some(provider)),
            Err(e) => warn!(
                "[STORAGE_PG] project {} model_provider decode failed (skipped field): {e}",
                row.project_id
            ),
        }
    }
    if let Some(value) = row.agent_status.clone() {
        match serde_json::from_value::<AgentStatus>(value) {
            Ok(status) => info.set_status(Some(status)),
            Err(e) => warn!(
                "[STORAGE_PG] project {} agent_status decode failed (skipped field): {e}",
                row.project_id
            ),
        }
    }
    if let Some(name) = row.container_name.as_deref()
        && let Some(basic) = container_by_name.get(name)
    {
        info.set_container(Some(basic.clone()));
    }
    Some(info)
}

/// 全量快照应用到镜像（启动加载与 sync 共用；latest 最后回放复原 latest_session）
pub(super) fn apply_snapshot(
    inner: &ProjectAdapter,
    container_by_name: HashMap<String, ContainerBasicInfo>,
    projects: Vec<ProjectRow>,
    sessions: Vec<SessionRow>,
) {
    let mut sessions_by_project: HashMap<String, Vec<String>> = HashMap::new();
    for row in sessions {
        sessions_by_project
            .entry(row.project_id)
            .or_default()
            .push(row.session_id);
    }
    let total_sessions: usize = sessions_by_project.values().map(Vec::len).sum();
    let mut loaded = 0usize;
    let mut skipped = 0usize;
    for row in projects {
        let Some(info) = hydrate_project(&row, &container_by_name) else {
            skipped += 1;
            continue;
        };
        let project_sessions = sessions_by_project
            .remove(&row.project_id)
            .unwrap_or_default();
        // latest 最后回放，保证 latest_session 复原
        let (latest, rest): (Vec<&String>, Vec<&String>) = project_sessions
            .iter()
            .partition(|sid| Some(AsRef::<str>::as_ref(*sid)) == row.latest_session.as_deref());
        // 先 insert 占位（无 session），再逐个 add（latest 放最后）
        if let Err(e) = inner.insert(row.project_id.clone(), Arc::new(info)) {
            warn!(
                "[STORAGE_PG] mirror insert failed for {}: {e:#}（schema 演进？跳过）",
                row.project_id
            );
            skipped += 1;
            continue;
        }
        for sid in rest {
            // restore 变体：回放不刷 last_activity/容器活跃（idle 计时不因重启归零）
            inner.restore_session_to_project(&row.project_id, sid);
        }
        for sid in latest {
            inner.restore_session_to_project(&row.project_id, sid);
        }
        loaded += 1;
    }
    info!(
        "[STORAGE_PG] snapshot applied: {loaded} projects (skipped {skipped}), {} containers, {total_sessions} sessions",
        container_by_name.len(),
    );
}

fn parse_service_type(s: Option<&str>) -> Option<ServiceType> {
    use std::str::FromStr as _;
    s.and_then(|value: &str| ServiceType::from_str(value).ok())
}

// ========== SSE 回源直查（miss → PG 单查 → hydrate 镜像） ==========

impl crate::pg::PgStore {
    /// 按 session_id 读：内存镜像 hit 直接返回；miss 回源直查主库一次
    /// （所有副本连 `-rw` 主库，无复制延迟——正常路径 durable 提交后必中），
    /// 命中则 hydrate 进本地镜像（旁路持久化，此后走内存）。
    pub async fn get_by_session_id_with_fetch(
        &self,
        session_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        if let Some(hit) = self.inner.get_by_session_id(session_id) {
            return Some(hit);
        }
        let fetched = match repo::fetch_project_by_session(&self.pool, session_id).await {
            Ok(Some(rows)) => rows,
            Ok(None) => return None, // 真 miss（session 不存在）
            Err(e) => {
                // DB 错误 ≠ miss：区分记录，避免把存储故障误判为"流量打到错误副本"
                tracing::warn!(
                    "[STORAGE_PG] session backfill fetch failed (treating as miss): session_id={}, error={e}",
                    session_id
                );
                return None;
            }
        };
        let hydrated = self.hydrate_fetched(session_id, fetched);
        tracing::info!(
            "[STORAGE_PG] session miss backfilled from PG: session_id={}, project_id={}",
            session_id,
            hydrated.project_id()
        );
        Some(hydrated)
    }

    /// 回源行组装为 info 并旁路写入内存镜像（数据本就来自 PG，回写不走持久化，
    /// 与 sync.rs 的 hydrate 同模式——复用 load 的组装逻辑）。
    /// `fetched_by` 是触发回源的 session_id（可能与 latest_session 不同）——
    /// 必须一并补进镜像，否则该 session 键下次查询仍 miss（回源缓存失效）
    fn hydrate_fetched(
        &self,
        fetched_by: &str,
        (project_row, container_row): (ProjectRow, Option<ContainerRow>),
    ) -> Arc<ProjectAndContainerInfo> {
        let mut container_by_name = HashMap::new();
        if let Some(row) = container_row {
            let name = row.container_name.clone();
            let basic = container_row_to_basic(&row);
            container_by_name.insert(name, basic);
        }
        // 解析失败（枚举演进/缺 service_type）→ 兜底空 map：项目记录仍返回
        //（SSE 只需要 project_id/container_name 路由信息）
        let mut info = hydrate_project(&project_row, &container_by_name)
            .unwrap_or_else(|| ProjectAndContainerInfo::new(project_row.project_id.clone()));
        // merge 语义（与 sync_once 一致）：整条 insert 会把本地镜像已有的其他
        // session 键抛掉——同 project 多 session 并发回源时互相驱逐，回源缓存
        // 永不生效（每消息一次主库查询）
        if let Some(existing) = self.inner.get(&project_row.project_id) {
            for sid in existing.sessions().iter() {
                info.restore_session(sid.clone());
            }
        }
        let info = Arc::new(info);
        // 旁路写入镜像（数据来自 PG，不走持久化）
        let pid = info.project_id().to_string();
        if let Err(e) = self.inner.insert_with_session(pid, Arc::clone(&info), None) {
            warn!(
                "[STORAGE_PG] backfill mirror insert failed: project_id={}, err={e:#}",
                project_row.project_id
            );
        }
        // latest_session 与回源键（可能不同）都补进镜像，下次任一键查询走内存
        //（restore：不刷 last_activity/容器活跃）
        if let Some(sid) = project_row.latest_session.as_deref() {
            self.inner.restore_session_to_project(info.project_id(), sid);
        }
        self.inner
            .restore_session_to_project(info.project_id(), fetched_by);
        info
    }
}

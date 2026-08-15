//! projects / containers / sessions 三表的数据访问（自 writer.rs 与 load.rs 迁入）
//!
//! 写语句与 PersistOp 一一对应（幂等 upsert / delete，writer 整批重放安全）；
//! 读语句服务启动全量加载。执行器泛型 `impl PgExecutor`（官方范式）。

use sqlx::{PgConnection, PgExecutor};

use crate::persist_ops::{ContainerSnapshot, ProjectSnapshot};

use super::rows::{ContainerRow, ProjectRow, SessionRow};

// ========== containers ==========

/// 容器整行 upsert（version 自增，Phase 2 乐观锁用）
pub(in crate::pg) async fn upsert_container<'e>(
    db: impl PgExecutor<'e>,
    c: &ContainerSnapshot,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO containers
           (container_name, container_id, logical_id, service_type, container_ip,
            internal_port, external_port, status, service_url, last_activity, created_at, version)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,1)
           ON CONFLICT (container_name) DO UPDATE SET
             container_id=EXCLUDED.container_id, logical_id=EXCLUDED.logical_id,
             service_type=EXCLUDED.service_type, container_ip=EXCLUDED.container_ip,
             internal_port=EXCLUDED.internal_port, external_port=EXCLUDED.external_port,
             status=EXCLUDED.status, service_url=EXCLUDED.service_url,
             last_activity=EXCLUDED.last_activity, created_at=EXCLUDED.created_at,
             version=containers.version+1"#,
    )
    .bind(&c.container_name)
    .bind(&c.container_id)
    .bind(&c.logical_id)
    .bind(&c.service_type)
    .bind(&c.container_ip)
    .bind(c.internal_port)
    .bind(c.external_port)
    .bind(&c.status)
    .bind(&c.service_url)
    .bind(c.last_activity)
    .bind(c.created_at)
    .execute(db)
    .await?;
    Ok(())
}

/// 刷新容器活跃时间（Touch，节流后由 writer 调用）
pub(in crate::pg) async fn touch_container<'e>(
    db: impl PgExecutor<'e>,
    container_name: &str,
    last_activity: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE containers SET last_activity=$2, version=version+1 WHERE container_name=$1",
    )
    .bind(container_name)
    .bind(last_activity)
    .execute(db)
    .await?;
    Ok(())
}

/// 按容器 ID 删除容器行及其全部关联 project 行（delete_container_with_projects 的
/// 持久化侧；sessions 经 FK 级联删除）
pub(in crate::pg) async fn delete_container_with_projects(
    db: &mut PgConnection,
    container_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM projects WHERE container_name IN \
         (SELECT container_name FROM containers WHERE container_id = $1)",
    )
    .bind(container_id)
    .execute(&mut *db)
    .await?;
    sqlx::query("DELETE FROM containers WHERE container_id = $1")
        .bind(container_id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// 全量容器行（启动加载）
pub(in crate::pg) async fn fetch_all_containers<'e>(
    db: impl PgExecutor<'e>,
) -> Result<Vec<ContainerRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT container_name, container_id, logical_id, service_type, container_ip, \
         internal_port, external_port, status, service_url, last_activity, created_at \
         FROM containers",
    )
    .fetch_all(db)
    .await
}

// ========== projects ==========

/// project 整行 upsert（version 自增）
pub(in crate::pg) async fn upsert_project<'e>(
    db: impl PgExecutor<'e>,
    p: &ProjectSnapshot,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO projects
           (project_id, user_id, pod_id, tenant_id, space_id, isolation_type,
            container_name, latest_session, model_provider, request_id, agent_status,
            service_type, last_activity, created_at, version)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,1)
           ON CONFLICT (project_id) DO UPDATE SET
             user_id=EXCLUDED.user_id, pod_id=EXCLUDED.pod_id,
             tenant_id=EXCLUDED.tenant_id, space_id=EXCLUDED.space_id,
             isolation_type=EXCLUDED.isolation_type,
             container_name=EXCLUDED.container_name,
             latest_session=EXCLUDED.latest_session,
             model_provider=EXCLUDED.model_provider, request_id=EXCLUDED.request_id,
             agent_status=EXCLUDED.agent_status, service_type=EXCLUDED.service_type,
             last_activity=EXCLUDED.last_activity, created_at=EXCLUDED.created_at,
             version=projects.version+1"#,
    )
    .bind(&p.project_id)
    .bind(&p.user_id)
    .bind(&p.pod_id)
    .bind(&p.tenant_id)
    .bind(&p.space_id)
    .bind(&p.isolation_type)
    .bind(&p.container_name)
    .bind(&p.latest_session)
    .bind(&p.model_provider)
    .bind(&p.request_id)
    .bind(&p.agent_status)
    .bind(&p.service_type)
    .bind(p.last_activity)
    .bind(p.created_at)
    .execute(db)
    .await?;
    Ok(())
}

/// 删除 project（sessions 经 FK ON DELETE CASCADE 级联）
pub(in crate::pg) async fn remove_project<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM projects WHERE project_id = $1")
        .bind(project_id)
        .execute(db)
        .await?;
    Ok(())
}

/// 刷新 project 活跃时间（Touch）
pub(in crate::pg) async fn touch_project<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
    last_activity: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE projects SET last_activity=$2, version=version+1 WHERE project_id=$1")
        .bind(project_id)
        .bind(last_activity)
        .execute(db)
        .await?;
    Ok(())
}

/// 更新 agent 状态快照（UpdateAgentStatus）
pub(in crate::pg) async fn update_agent_status<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
    agent_status: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE projects SET agent_status=$2, version=version+1 WHERE project_id=$1")
        .bind(project_id)
        .bind(agent_status)
        .execute(db)
        .await?;
    Ok(())
}

/// 全量 project 行（启动加载）
pub(in crate::pg) async fn fetch_all_projects<'e>(
    db: impl PgExecutor<'e>,
) -> Result<Vec<ProjectRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT project_id, user_id, pod_id, tenant_id, space_id, isolation_type, \
         container_name, latest_session, model_provider, request_id, agent_status, \
         service_type, last_activity, created_at FROM projects",
    )
    .fetch_all(db)
    .await
}

// ========== sessions ==========

/// 登记 session（upsert：重复登记即刷新归属与冗余容器名）
pub(in crate::pg) async fn add_session<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
    session_id: &str,
    container_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO sessions (session_id, project_id, container_name)
           VALUES ($1,$2,$3)
           ON CONFLICT (session_id) DO UPDATE SET
             project_id=EXCLUDED.project_id,
             container_name=EXCLUDED.container_name,
             last_seen_at=now()"#,
    )
    .bind(session_id)
    .bind(project_id)
    .bind(container_name)
    .execute(db)
    .await?;
    Ok(())
}

/// 移除单个 session
pub(in crate::pg) async fn remove_session<'e>(
    db: impl PgExecutor<'e>,
    session_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

/// 清空 project 的全部 session
pub(in crate::pg) async fn clear_sessions<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE project_id = $1")
        .bind(project_id)
        .execute(db)
        .await?;
    Ok(())
}

/// 刷新 session 活跃时间（TouchSession）
pub(in crate::pg) async fn touch_session<'e>(
    db: impl PgExecutor<'e>,
    session_id: &str,
    last_seen_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET last_seen_at=$2 WHERE session_id=$1")
        .bind(session_id)
        .bind(last_seen_at)
        .execute(db)
        .await?;
    Ok(())
}

/// 全量 session 行（启动加载）
pub(in crate::pg) async fn fetch_all_sessions<'e>(
    db: impl PgExecutor<'e>,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    sqlx::query_as("SELECT session_id, project_id, container_name FROM sessions")
        .fetch_all(db)
        .await
}

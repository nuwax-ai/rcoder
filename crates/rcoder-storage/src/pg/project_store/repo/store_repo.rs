//! projects / containers / sessions 三表的数据访问（自 writer.rs 与 load.rs 迁入）
//!
//! 写语句与 PersistOp 一一对应（幂等 upsert / delete，writer 整批重放安全）；
//! 读语句服务启动全量加载。执行器泛型 `impl PgExecutor`（官方范式）。

use shared_types::persistence::PersistenceOperationOutcome;
use sqlx::{PgConnection, PgExecutor};

fn outcome(rows: u64) -> PersistenceOperationOutcome {
    if rows == 0 {
        PersistenceOperationOutcome::Superseded
    } else {
        PersistenceOperationOutcome::Committed
    }
}

use crate::pg::project_store::persist_ops::{ContainerSnapshot, ProjectSnapshot};

use super::rows::{ContainerRow, ProjectRow, SessionRow};

// ========== containers ==========

/// 容器整行 upsert（version 自增，Phase 2 乐观锁用）
pub(in crate::pg) async fn upsert_container<'e>(
    db: impl PgExecutor<'e>,
    c: &ContainerSnapshot,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    let result = sqlx::query(
        // 防回退守卫（与 upsert_project 同构）：durable 降级快照与 write-behind
        // 重放可交错——created_at 跨容器重建单调（同容器名的重建必是新时刻），
        // 旧快照条件不满足被跳过，防止 container_ip/status/service_url 回退
        //（SSE/Pingora 按 PG 回源 hydrate 后会路由到旧 IP）。
        // 相等放行：同容器的正常字段更新（IP 漂移等）created_at 不变。
        r#"INSERT INTO containers
           (container_name, container_id, logical_id, service_type, container_ip,
            internal_port, external_port, status, service_url, last_activity, created_at, version)
           SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,1
           WHERE NOT EXISTS (SELECT 1 FROM container_tombstones WHERE container_id=$2)
           ON CONFLICT (container_name) DO UPDATE SET
             container_id=EXCLUDED.container_id, logical_id=EXCLUDED.logical_id,
             service_type=EXCLUDED.service_type, container_ip=EXCLUDED.container_ip,
             internal_port=EXCLUDED.internal_port, external_port=EXCLUDED.external_port,
             status=EXCLUDED.status, service_url=EXCLUDED.service_url,
             last_activity=EXCLUDED.last_activity, created_at=EXCLUDED.created_at,
             version=containers.version+1
           WHERE EXCLUDED.created_at >= containers.created_at"#,
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
    Ok(outcome(result.rows_affected()))
}

/// 刷新容器活跃时间（Touch，节流后由 writer 调用）
pub(in crate::pg) async fn touch_container<'e>(
    db: impl PgExecutor<'e>,
    container_name: &str,
    last_activity: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    // 单调条件：积压重放的旧 Touch 不得把 last_activity 拉回（保持
    // upsert_* 防回退守卫所依赖的单调前提）
    sqlx::query(
        "UPDATE containers SET last_activity=$2, version=version+1 \
         WHERE container_name=$1 AND $2 > last_activity",
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
    projects: &[(String, String)],
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    sqlx::query("INSERT INTO container_tombstones(container_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(container_id)
        .execute(&mut *db)
        .await?;
    let mut applied = false;
    for (project_id, generation) in projects {
        // Retire only the project incarnation captured when deletion was accepted.
        let still_owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects p JOIN containers c ON c.container_name=p.container_name WHERE p.project_id=$1 AND p.generation=$2 AND c.container_id=$3)")
            .bind(project_id).bind(generation).bind(container_id).fetch_one(&mut *db).await?;
        if still_owned {
            applied |= remove_project(&mut *db, project_id, generation).await?
                == PersistenceOperationOutcome::Committed;
        }
    }
    let result = sqlx::query("DELETE FROM containers WHERE container_id=$1")
        .bind(container_id)
        .execute(&mut *db)
        .await?;
    Ok(if applied {
        PersistenceOperationOutcome::Committed
    } else {
        outcome(result.rows_affected())
    })
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

/// Identity-fenced project upsert; zero affected rows explicitly report Superseded.
pub(in crate::pg) async fn upsert_project(
    db: &mut PgConnection,
    p: &ProjectSnapshot,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    if let Some(previous) = &p.predecessor {
        sqlx::query("INSERT INTO project_tombstones(project_id,generation) SELECT $1,$2 WHERE NOT EXISTS (SELECT 1 FROM project_tombstones WHERE project_id=$1 AND generation=$3) ON CONFLICT DO NOTHING")
            .bind(&p.project_id).bind(previous).bind(&p.generation).execute(&mut *db).await?;
        sqlx::query("DELETE FROM sessions WHERE project_id=$1 AND project_generation=$2 AND EXISTS(SELECT 1 FROM project_tombstones WHERE project_id=$1 AND generation=$2)")
            .bind(&p.project_id).bind(previous).execute(&mut *db).await?;
    }
    let result = sqlx::query(
        // Incarnation changes require an explicit predecessor. Tombstones permanently
        // fence delayed writes from retired generations, independently of wall clocks.
        // Within one live generation, retain the existing last_activity ordering;
        // equal timestamps use SQL commit order (this is not a revision CAS).
        r#"INSERT INTO projects
           (project_id, user_id, pod_id, tenant_id, space_id, isolation_type,
            container_name, latest_session, model_provider, request_id, agent_status,
            service_type, last_activity, created_at, generation, version)
           SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,1
           WHERE NOT EXISTS (SELECT 1 FROM project_tombstones WHERE project_id=$1 AND generation=$15)
           ON CONFLICT (project_id) DO UPDATE SET
             user_id=EXCLUDED.user_id, pod_id=EXCLUDED.pod_id,
             tenant_id=EXCLUDED.tenant_id, space_id=EXCLUDED.space_id,
             isolation_type=EXCLUDED.isolation_type,
             container_name=EXCLUDED.container_name,
             latest_session=EXCLUDED.latest_session,
             model_provider=EXCLUDED.model_provider, request_id=EXCLUDED.request_id,
             agent_status=EXCLUDED.agent_status, service_type=EXCLUDED.service_type,
             last_activity=EXCLUDED.last_activity, created_at=EXCLUDED.created_at,
             generation=EXCLUDED.generation, version=projects.version+1
           WHERE (projects.generation=EXCLUDED.generation AND EXCLUDED.last_activity >= projects.last_activity)
              OR projects.generation=$16"#,
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
    .bind(&p.generation)
    .bind(&p.predecessor)
    .execute(db)
    .await?;
    Ok(outcome(result.rows_affected()))
}

/// 删除 project（sessions 经 FK ON DELETE CASCADE 级联）
pub(in crate::pg) async fn remove_project(
    db: &mut PgConnection,
    project_id: &str,
    generation: &str,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    sqlx::query("INSERT INTO project_tombstones(project_id,generation) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(project_id).bind(generation).execute(&mut *db).await?;
    let result = sqlx::query("DELETE FROM projects WHERE project_id=$1 AND generation=$2")
        .bind(project_id)
        .bind(generation)
        .execute(&mut *db)
        .await?;
    Ok(outcome(result.rows_affected()))
}

/// The caller locks the captured container name and project before this predicate.
pub(in crate::pg) async fn remove_project_for_container(
    db: &mut PgConnection,
    project_id: &str,
    generation: &str,
    container_id: &str,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects p JOIN containers c ON c.container_name=p.container_name WHERE p.project_id=$1 AND p.generation=$2 AND c.container_id=$3)")
        .bind(project_id).bind(generation).bind(container_id).fetch_one(&mut *db).await?;
    if !matches {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    remove_project(db, project_id, generation).await
}

/// 刷新 project 活跃时间（Touch）
pub(in crate::pg) async fn touch_project<'e>(
    db: impl PgExecutor<'e>,
    project_id: &str,
    last_activity: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    // 单调条件：积压重放的旧 Touch 不得把 last_activity 拉回（idle 判据不回摆）
    sqlx::query(
        "UPDATE projects SET last_activity=$2, version=version+1 \
         WHERE project_id=$1 AND $2 > last_activity",
    )
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
         service_type, last_activity, created_at, generation, (SELECT jsonb_object_agg(session_id,generation) FROM sessions WHERE project_id=projects.project_id AND project_generation=projects.generation) AS session_identities FROM projects",
    )
    .fetch_all(db)
    .await
}

// ========== sessions ==========

/// 登记 session（upsert：重复登记即刷新归属与冗余容器名）
pub(in crate::pg) async fn add_session(
    db: &mut PgConnection,
    project_id: &str,
    session_id: &str,
    container_name: Option<&str>,
    project_generation: &str,
    generation: &str,
    predecessor: Option<&str>,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    if let Some(previous) = predecessor {
        sqlx::query("INSERT INTO session_tombstones(session_id,generation) SELECT $1,$2 WHERE NOT EXISTS(SELECT 1 FROM session_tombstones WHERE session_id=$1 AND generation=$3) ON CONFLICT DO NOTHING")
            .bind(session_id).bind(previous).bind(generation).execute(&mut *db).await?;
    }
    let result = sqlx::query(
        r#"INSERT INTO sessions (session_id, project_id, container_name, project_generation, generation)
           SELECT $1,$2,$3,$4,$5
           WHERE EXISTS (SELECT 1 FROM projects WHERE project_id=$2 AND generation=$4)
             AND NOT EXISTS (SELECT 1 FROM session_tombstones WHERE session_id=$1 AND generation=$5)
           ON CONFLICT (session_id) DO UPDATE SET
             project_id=EXCLUDED.project_id,
             container_name=EXCLUDED.container_name,
             last_seen_at=now(), generation=EXCLUDED.generation, project_generation=EXCLUDED.project_generation
           WHERE (sessions.generation=EXCLUDED.generation AND sessions.project_generation=EXCLUDED.project_generation)
              OR sessions.generation=$6
              OR EXISTS(SELECT 1 FROM project_tombstones WHERE project_id=sessions.project_id AND generation=sessions.project_generation)"#,
    )
    .bind(session_id)
    .bind(project_id)
    .bind(container_name)
    .bind(project_generation)
    .bind(generation)
    .bind(predecessor)
    .execute(db)
    .await?;
    Ok(outcome(result.rows_affected()))
}

/// 移除单个 session
pub(in crate::pg) async fn remove_session(
    db: &mut PgConnection,
    session_id: &str,
    generation: &str,
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    sqlx::query("INSERT INTO session_tombstones(session_id,generation) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(session_id).bind(generation).execute(&mut *db).await?;
    let result = sqlx::query("DELETE FROM sessions WHERE session_id=$1 AND generation=$2")
        .bind(session_id)
        .bind(generation)
        .execute(&mut *db)
        .await?;
    Ok(outcome(result.rows_affected()))
}

/// Clear only session identities captured at admission, never later additions.
pub(in crate::pg) async fn clear_sessions(
    db: &mut PgConnection,
    _project_id: &str,
    _generation: &str,
    sessions: &[(String, String)],
) -> Result<PersistenceOperationOutcome, sqlx::Error> {
    let mut applied = false;
    for (id, generation) in sessions {
        applied |= remove_session(&mut *db, id, generation).await?
            == PersistenceOperationOutcome::Committed;
    }
    Ok(if applied {
        PersistenceOperationOutcome::Committed
    } else {
        PersistenceOperationOutcome::Superseded
    })
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
    sqlx::query_as("SELECT session_id, project_id, container_name, generation, project_generation FROM sessions")
        .fetch_all(db)
        .await
}

// ========== 按 session 单查（SSE 回源直查用） ==========

/// sessions 表 PK 直查 → 该 project 的完整行（projects + containers）
///
/// SSE lookup miss 的回源路径：所有副本连 -rw 主库，durable 提交后必中。
/// 返回 (ProjectRow, Option<ContainerRow>)——容器行可缺（FK ON DELETE SET NULL
/// 或占位条目），hydrate_project 容忍缺容器。
/// sessions 表 PK 直查 → 该 project 的完整行（projects + containers）。
///
/// SSE lookup miss 的回源路径：所有副本连 -rw 主库，durable 提交后必中。
/// 返回 Result 区分 DB 错误与真 miss（调用方需分别处理：错误记日志告警，
/// miss 才是"session 不存在"）。
pub(in crate::pg) async fn fetch_project_by_session(
    db: &sqlx::PgPool,
    session_id: &str,
) -> Result<Option<(ProjectRow, Option<ContainerRow>)>, sqlx::Error> {
    let Some(project) = sqlx::query_as::<_, ProjectRow>(
        "SELECT p.project_id, p.user_id, p.pod_id, p.tenant_id, p.space_id, \
         p.isolation_type, p.container_name, p.latest_session, p.model_provider, \
         p.request_id, p.agent_status, p.service_type, p.last_activity, p.created_at, p.generation, \
         (SELECT jsonb_object_agg(session_id,generation) FROM sessions WHERE project_id=p.project_id AND project_generation=p.generation) AS session_identities \
         FROM projects p \
         WHERE p.project_id = (SELECT project_id FROM sessions WHERE session_id = $1)",
    )
    .bind(session_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    let container = match project.container_name.as_deref() {
        Some(name) => {
            sqlx::query_as::<_, ContainerRow>(
                "SELECT container_name, container_id, logical_id, service_type, \
                 container_ip, internal_port, external_port, status, service_url, \
                 last_activity, created_at FROM containers WHERE container_name = $1",
            )
            .bind(name)
            .fetch_optional(db)
            .await?
        }
        None => None,
    };
    Ok(Some((project, container)))
}

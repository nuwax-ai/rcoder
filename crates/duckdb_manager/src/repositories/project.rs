//! 项目 Repository
//!
//! 提供项目表的数据访问操作

use crate::connection::DuckDbConnection;
use crate::error::{DuckDbError, DuckDbResult};
use crate::models::ProjectRecord;
use chrono::{DateTime, Utc};
use duckdb::params;
use shared_types::ServiceType;

/// 项目 Repository
pub struct ProjectRepository {
    conn: DuckDbConnection,
}

impl ProjectRepository {
    /// 创建新的 ProjectRepository
    pub fn new(conn: DuckDbConnection) -> Self {
        Self { conn }
    }

    /// 插入或更新项目记录
    pub fn upsert(&self, record: &ProjectRecord) -> DuckDbResult<()> {
        let service_type_str = record.service_type.to_string();
        let created_at_str = record.created_at.to_rfc3339();
        let last_activity_str = record.last_activity.to_rfc3339();
        let session_created_at_str = record.session_created_at.map(|dt| dt.to_rfc3339());
        let session_last_activity_str = record.session_last_activity.map(|dt| dt.to_rfc3339());

        self.conn.with_connection(|c| {
            c.execute(
                r#"
                INSERT OR REPLACE INTO projects (
                    project_id, session_id, service_type, container_id,
                    user_id, pod_id, agent_status_code, agent_status_name,
                    request_id, model_provider_json, created_at, last_activity,
                    session_created_at, session_last_activity
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                params![
                    record.project_id,
                    record.session_id,
                    service_type_str,
                    record.container_id,
                    record.user_id,
                    record.pod_id,
                    record.agent_status_code,
                    record.agent_status_name,
                    record.request_id,
                    record.model_provider_json,
                    created_at_str,
                    last_activity_str,
                    session_created_at_str,
                    session_last_activity_str,
                ],
            )?;
            Ok(())
        })
    }

    /// 根据项目ID查找项目
    pub fn find_by_id(&self, project_id: &str) -> DuckDbResult<Option<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE project_id = ?
                "#,
            )?;

            let mut rows = stmt.query(params![project_id])?;

            match rows.next()? {
                Some(row) => Ok(Some(Self::row_to_record(row)?)),
                None => Ok(None),
            }
        })
    }

    /// 根据会话ID查找项目
    pub fn find_by_session_id(&self, session_id: &str) -> DuckDbResult<Option<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE session_id = ?
                "#,
            )?;

            let mut rows = stmt.query(params![session_id])?;

            match rows.next()? {
                Some(row) => Ok(Some(Self::row_to_record(row)?)),
                None => Ok(None),
            }
        })
    }

    /// 根据用户ID查找所有项目 (ComputerAgentRunner 模式)
    ///
    /// 返回该用户的所有项目记录，按最后活动时间倒序排列
    pub fn find_projects_by_user_id(&self, user_id: &str) -> DuckDbResult<Vec<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE user_id = ?
                ORDER BY last_activity DESC
                "#,
            )?;

            let mut rows = stmt.query(params![user_id])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 获取用户最新活跃项目的容器ID (ComputerAgentRunner 核心用例)
    ///
    /// 在 ComputerAgentRunner 模式下，一个用户对应一个容器，
    /// 此方法返回该用户最近活跃项目关联的容器ID
    pub fn get_latest_container_id_by_user_id(
        &self,
        user_id: &str,
    ) -> DuckDbResult<Option<String>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT container_id
                FROM projects
                WHERE user_id = ?
                ORDER BY last_activity DESC
                LIMIT 1
                "#,
            )?;

            let mut rows = stmt.query(params![user_id])?;

            match rows.next()? {
                Some(row) => {
                    let container_id: String = row.get(0)?;
                    Ok(Some(container_id))
                }
                None => Ok(None),
            }
        })
    }

    /// 根据 pod_id 查找所有项目 (RCoder 共享容器模式)
    ///
    /// 在 RCoder 共享容器模式下，多个项目可能共享同一个容器（通过 pod_id 标识），
    /// 返回该 Pod 下所有项目记录，按最后活动时间倒序排列
    pub fn find_projects_by_pod_id(&self, pod_id: &str) -> DuckDbResult<Vec<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE pod_id = ?
                ORDER BY last_activity DESC
                "#,
            )?;

            let mut rows = stmt.query(params![pod_id])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 获取 Pod 最新活跃项目的容器ID (共享容器模式)
    ///
    /// 在共享容器模式下，多个项目可能共享同一个容器（通过 pod_id 标识），
    /// 此方法返回该 Pod 下最近活跃项目关联的容器ID
    pub fn get_latest_container_id_by_pod_id(
        &self,
        pod_id: &str,
    ) -> DuckDbResult<Option<String>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT container_id
                FROM projects
                WHERE pod_id = ?
                ORDER BY last_activity DESC
                LIMIT 1
                "#,
            )?;

            let mut rows = stmt.query(params![pod_id])?;

            match rows.next()? {
                Some(row) => {
                    let container_id: String = row.get(0)?;
                    Ok(Some(container_id))
                }
                None => Ok(None),
            }
        })
    }

    /// 根据容器ID查找所有关联的项目
    pub fn find_by_container_id(&self, container_id: &str) -> DuckDbResult<Vec<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE container_id = ?
                "#,
            )?;

            let mut rows = stmt.query(params![container_id])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 删除项目
    pub fn delete(&self, project_id: &str) -> DuckDbResult<bool> {
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "DELETE FROM projects WHERE project_id = ?",
                params![project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 根据容器ID删除所有关联的项目
    pub fn delete_by_container_id(&self, container_id: &str) -> DuckDbResult<usize> {
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "DELETE FROM projects WHERE container_id = ?",
                params![container_id],
            )?;
            Ok(affected)
        })
    }

    /// 检查项目是否存在
    pub fn exists(&self, project_id: &str) -> DuckDbResult<bool> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare("SELECT 1 FROM projects WHERE project_id = ? LIMIT 1")?;
            let mut rows = stmt.query(params![project_id])?;
            Ok(rows.next()?.is_some())
        })
    }

    /// 更新项目最后活动时间，返回实际更新使用的时间戳
    pub fn update_activity(&self, project_id: &str) -> DuckDbResult<Option<DateTime<Utc>>> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "UPDATE projects SET last_activity = ? WHERE project_id = ?",
                params![now_str, project_id],
            )?;
            Ok(if affected > 0 { Some(now) } else { None })
        })
    }

    /// 更新会话信息
    pub fn update_session(&self, project_id: &str, session_id: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET session_id = ?,
                    last_activity = ?,
                    session_created_at = COALESCE(session_created_at, ?),
                    session_last_activity = ?
                WHERE project_id = ?
                "#,
                params![session_id, now_str, now_str, now_str, project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 清除会话信息（将 session_id 设置为 NULL）
    ///
    /// 用于 Agent 停止后清理会话状态
    pub fn clear_session(&self, project_id: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET session_id = NULL,
                    last_activity = ?,
                    session_last_activity = ?
                WHERE project_id = ?
                "#,
                params![now_str, now_str, project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 更新会话活动时间
    pub fn update_session_activity(&self, session_id: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET last_activity = ?,
                    session_last_activity = ?
                WHERE session_id = ?
                "#,
                params![now_str, now_str, session_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 原子更新 Agent 状态（使用事务）
    ///
    /// 注意：此方法不会更新 last_activity，只更新状态字段
    /// 这是为了避免每 30 秒的状态检查刷新活动时间，导致闲置容器永远不会被清理
    pub fn update_status_atomic(
        &self,
        project_id: &str,
        status_code: i32,
        status_name: &str,
    ) -> DuckDbResult<bool> {
        self.conn.transaction(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET agent_status_code = ?,
                    agent_status_name = ?
                WHERE project_id = ?
                "#,
                params![status_code, status_name, project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 根据会话ID获取容器名称
    ///
    /// 与 `get_container_id_by_session` 不同，此方法返回 `container_name` 而不是 `container_id`。
    /// 容器名称是稳定的（如 `computer-agent-runner-user_123`），即使容器被重建，
    /// 可以通过 Docker API 查询到新容器。而 container_id 在容器重建后会改变。
    ///
    /// # 用途
    /// 用于 SSE 连接验证时，通过 container_name 实时查询 Docker API 获取容器状态。
    pub fn get_container_name_by_session(&self, session_id: &str) -> DuckDbResult<Option<String>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT c.container_name
                FROM projects p
                JOIN containers c ON p.container_id = c.container_id
                WHERE p.session_id = ?
                "#,
            )?;
            let mut rows = stmt.query(params![session_id])?;

            match rows.next()? {
                Some(row) => {
                    let container_name: String = row.get(0)?;
                    Ok(Some(container_name))
                }
                None => Ok(None),
            }
        })
    }

    /// 查找需要清理的项目（闲置超过指定分钟数）
    pub fn find_projects_for_cleanup(&self, idle_minutes: i64) -> DuckDbResult<Vec<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE DATEDIFF('minute', last_activity, NOW()) >= ?
                "#,
            )?;

            let mut rows = stmt.query(params![idle_minutes])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 获取所有项目
    pub fn find_all(&self) -> DuckDbResult<Vec<ProjectRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                ORDER BY created_at DESC
                "#,
            )?;

            let mut rows = stmt.query([])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 按服务类型查找项目
    pub fn find_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> DuckDbResult<Vec<ProjectRecord>> {
        let service_type_str = service_type.to_string();
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT project_id, session_id, service_type, container_id,
                       user_id, pod_id, agent_status_code, agent_status_name,
                       request_id, model_provider_json, created_at, last_activity,
                       session_created_at, session_last_activity
                FROM projects
                WHERE service_type = ?
                ORDER BY created_at DESC
                "#,
            )?;

            let mut rows = stmt.query(params![service_type_str])?;
            let mut records = Vec::new();

            while let Some(row) = rows.next()? {
                records.push(Self::row_to_record(row)?);
            }

            Ok(records)
        })
    }

    /// 获取项目数量统计
    pub fn count(&self) -> DuckDbResult<usize> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare("SELECT COUNT(*) FROM projects")?;
            let mut rows = stmt.query([])?;
            let row = rows
                .next()?
                .ok_or_else(|| DuckDbError::InternalError("unable to get project count".to_string()))?;
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        })
    }

    /// 获取活跃会话数量
    pub fn count_active_sessions(&self) -> DuckDbResult<usize> {
        self.conn.with_connection(|c| {
            let mut stmt =
                c.prepare("SELECT COUNT(*) FROM projects WHERE session_id IS NOT NULL")?;
            let mut rows = stmt.query([])?;
            let row = rows
                .next()?
                .ok_or_else(|| DuckDbError::InternalError("unable to get session count".to_string()))?;
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        })
    }

    /// 按服务类型统计项目数量
    pub fn count_by_service_type(
        &self,
    ) -> DuckDbResult<std::collections::HashMap<ServiceType, usize>> {
        self.conn.with_connection(|c| {
            let mut stmt =
                c.prepare("SELECT service_type, COUNT(*) FROM projects GROUP BY service_type")?;
            let mut rows = stmt.query([])?;
            let mut counts = std::collections::HashMap::new();

            while let Some(row) = rows.next()? {
                let service_type_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;

                if let Ok(service_type) = service_type_str.parse::<ServiceType>() {
                    counts.insert(service_type, count as usize);
                }
            }

            Ok(counts)
        })
    }

    /// 更新模型提供商配置
    pub fn update_model_provider(
        &self,
        project_id: &str,
        model_provider_json: Option<&str>,
    ) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET model_provider_json = ?,
                    last_activity = ?
                WHERE project_id = ?
                "#,
                params![model_provider_json, now_str, project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 更新请求ID
    pub fn update_request_id(
        &self,
        project_id: &str,
        request_id: Option<&str>,
    ) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE projects
                SET request_id = ?,
                    last_activity = ?
                WHERE project_id = ?
                "#,
                params![request_id, now_str, project_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 从数据库行转换为 ProjectRecord
    fn row_to_record(row: &duckdb::Row<'_>) -> DuckDbResult<ProjectRecord> {
        let project_id: String = row.get(0)?;
        let session_id: Option<String> = row.get(1)?;
        let service_type_str: String = row.get(2)?;
        let container_id: String = row.get(3)?;
        let user_id: Option<String> = row.get(4)?;
        let pod_id: Option<String> = row.get(5)?;
        let agent_status_code: Option<i32> = row.get(6)?;
        let agent_status_name: Option<String> = row.get(7)?;
        let request_id: Option<String> = row.get(8)?;
        let model_provider_json: Option<String> = row.get(9)?;

        // DuckDB 返回 TIMESTAMP 类型，需要使用 get_ref 并转换
        let created_at = Self::get_timestamp_from_row(row, 10)?;
        let last_activity = Self::get_timestamp_from_row(row, 11)?;
        let session_created_at = Self::get_optional_timestamp_from_row(row, 12)?;
        let session_last_activity = Self::get_optional_timestamp_from_row(row, 13)?;

        let service_type = service_type_str
            .parse::<ServiceType>()
            .map_err(|e| DuckDbError::InternalError(format!("failed to parse service type: {}", e)))?;

        Ok(ProjectRecord {
            project_id,
            session_id,
            service_type,
            container_id,
            user_id,
            pod_id,
            agent_status_code,
            agent_status_name,
            request_id,
            model_provider_json,
            created_at,
            last_activity,
            session_created_at,
            session_last_activity,
        })
    }

    /// 从行中获取时间戳
    fn get_timestamp_from_row(row: &duckdb::Row<'_>, idx: usize) -> DuckDbResult<DateTime<Utc>> {
        use duckdb::types::ValueRef;

        let value_ref = row.get_ref(idx)?;
        match value_ref {
            ValueRef::Timestamp(_, micros) => {
                let secs = micros / 1_000_000;
                let nsecs = ((micros % 1_000_000) * 1000) as u32;
                Ok(DateTime::from_timestamp(secs, nsecs).unwrap_or_else(Utc::now))
            }
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)
                    .map_err(|e| DuckDbError::InternalError(format!("UTF8 parsing failed: {}", e)))?;
                DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| DuckDbError::InternalError(format!("timestamp parsing failed: {}", e)))
            }
            _ => Ok(Utc::now()),
        }
    }

    /// 从行中获取可选时间戳
    fn get_optional_timestamp_from_row(
        row: &duckdb::Row<'_>,
        idx: usize,
    ) -> DuckDbResult<Option<DateTime<Utc>>> {
        use duckdb::types::ValueRef;

        let value_ref = row.get_ref(idx)?;
        match value_ref {
            ValueRef::Null => Ok(None),
            ValueRef::Timestamp(_, micros) => {
                let secs = micros / 1_000_000;
                let nsecs = ((micros % 1_000_000) * 1000) as u32;
                Ok(Some(
                    DateTime::from_timestamp(secs, nsecs).unwrap_or_else(Utc::now),
                ))
            }
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)
                    .map_err(|e| DuckDbError::InternalError(format!("UTF8 parsing failed: {}", e)))?;
                DateTime::parse_from_rfc3339(s)
                    .map(|dt| Some(dt.with_timezone(&Utc)))
                    .map_err(|e| DuckDbError::InternalError(format!("timestamp parsing failed: {}", e)))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaInitializer;

    fn setup_test_db() -> ProjectRepository {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        SchemaInitializer::initialize(&conn).unwrap();
        ProjectRepository::new(conn)
    }

    fn create_test_record(id: &str) -> ProjectRecord {
        ProjectRecord::new(
            id.to_string(),
            ServiceType::RCoder,
            format!("container-{}", id),
        )
    }

    #[test]
    fn test_upsert_and_find() {
        let repo = setup_test_db();
        let record = create_test_record("p1");

        repo.upsert(&record).unwrap();

        let found = repo.find_by_id("p1").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.project_id, "p1");
        assert_eq!(found.container_id, "container-p1");
    }

    #[test]
    fn test_update_session() {
        let repo = setup_test_db();
        let record = create_test_record("p1");

        repo.upsert(&record).unwrap();
        repo.update_session("p1", "session-1").unwrap();

        let found = repo.find_by_id("p1").unwrap().unwrap();
        assert_eq!(found.session_id, Some("session-1".to_string()));
        assert!(found.session_created_at.is_some());
    }

    #[test]
    fn test_find_by_session_id() {
        let repo = setup_test_db();
        let record = create_test_record("p1");

        repo.upsert(&record).unwrap();
        repo.update_session("p1", "session-1").unwrap();

        let found = repo.find_by_session_id("session-1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().project_id, "p1");
    }

    #[test]
    fn test_update_status_atomic() {
        let repo = setup_test_db();
        let record = create_test_record("p1");

        repo.upsert(&record).unwrap();
        repo.update_status_atomic("p1", 1, "running").unwrap();

        let found = repo.find_by_id("p1").unwrap().unwrap();
        assert_eq!(found.agent_status_code, Some(1));
        assert_eq!(found.agent_status_name, Some("running".to_string()));
    }

    #[test]
    fn test_delete() {
        let repo = setup_test_db();
        let record = create_test_record("p1");

        repo.upsert(&record).unwrap();
        assert!(repo.exists("p1").unwrap());

        repo.delete("p1").unwrap();
        assert!(!repo.exists("p1").unwrap());
    }

    #[test]
    fn test_delete_by_container_id() {
        let repo = setup_test_db();

        let mut p1 = create_test_record("p1");
        p1.container_id = "c1".to_string();
        let mut p2 = create_test_record("p2");
        p2.container_id = "c1".to_string();
        let mut p3 = create_test_record("p3");
        p3.container_id = "c2".to_string();

        repo.upsert(&p1).unwrap();
        repo.upsert(&p2).unwrap();
        repo.upsert(&p3).unwrap();

        let deleted = repo.delete_by_container_id("c1").unwrap();
        assert_eq!(deleted, 2);

        assert!(!repo.exists("p1").unwrap());
        assert!(!repo.exists("p2").unwrap());
        assert!(repo.exists("p3").unwrap());
    }

    #[test]
    fn test_count_active_sessions() {
        let repo = setup_test_db();

        repo.upsert(&create_test_record("p1")).unwrap();
        repo.upsert(&create_test_record("p2")).unwrap();
        repo.upsert(&create_test_record("p3")).unwrap();

        assert_eq!(repo.count_active_sessions().unwrap(), 0);

        repo.update_session("p1", "session-1").unwrap();
        repo.update_session("p2", "session-2").unwrap();

        assert_eq!(repo.count_active_sessions().unwrap(), 2);
    }

    #[test]
    fn test_find_projects_by_user_id() {
        let repo = setup_test_db();

        let record1 = ProjectRecord::new_with_user_id(
            "p1".to_string(),
            "user-1".to_string(),
            ServiceType::ComputerAgentRunner,
            "c1".to_string(),
        );

        let record2 = ProjectRecord::new_with_user_id(
            "p2".to_string(),
            "user-1".to_string(),
            ServiceType::ComputerAgentRunner,
            "c1".to_string(),
        );

        repo.upsert(&record1).unwrap();
        repo.upsert(&record2).unwrap();

        let found = repo.find_projects_by_user_id("user-1").unwrap();
        assert_eq!(found.len(), 2);

        // 验证按 last_activity 倒序排列
        let project_ids: Vec<&str> = found.iter().map(|r| r.project_id.as_str()).collect();
        assert!(project_ids.contains(&"p1"));
        assert!(project_ids.contains(&"p2"));
    }

    #[test]
    fn test_get_latest_container_id_by_user_id() {
        let repo = setup_test_db();

        // 用户没有项目时返回 None
        let container_id = repo.get_latest_container_id_by_user_id("user-1").unwrap();
        assert!(container_id.is_none());

        // 创建项目
        let record = ProjectRecord::new_with_user_id(
            "p1".to_string(),
            "user-1".to_string(),
            ServiceType::ComputerAgentRunner,
            "c1".to_string(),
        );
        repo.upsert(&record).unwrap();

        // 现在应该返回容器ID
        let container_id = repo.get_latest_container_id_by_user_id("user-1").unwrap();
        assert_eq!(container_id, Some("c1".to_string()));
    }

    #[test]
    fn test_get_latest_container_id_by_pod_id() {
        let repo = setup_test_db();

        // Pod 没有项目时返回 None
        let container_id = repo.get_latest_container_id_by_pod_id("pod-1").unwrap();
        assert!(container_id.is_none());

        // 创建带 pod_id 的项目
        let mut record = ProjectRecord::new_with_user_id(
            "p1".to_string(),
            "user-1".to_string(),
            ServiceType::ComputerAgentRunner,
            "c1".to_string(),
        );
        record.pod_id = Some("pod-1".to_string());
        repo.upsert(&record).unwrap();

        // 现在应该返回容器ID
        let container_id = repo.get_latest_container_id_by_pod_id("pod-1").unwrap();
        assert_eq!(container_id, Some("c1".to_string()));
    }
}

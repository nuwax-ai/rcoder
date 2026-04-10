//! 容器 Repository
//!
//! 提供容器表的数据访问操作

use crate::connection::DuckDbConnection;
use crate::error::{DuckDbError, DuckDbResult};
use crate::models::{ContainerRecord, IdleContainerInfo};
use chrono::{DateTime, Utc};
use duckdb::params;
use shared_types::ServiceType;

/// 容器 Repository
pub struct ContainerRepository {
    conn: DuckDbConnection,
}

impl ContainerRepository {
    /// 创建新的 ContainerRepository
    pub fn new(conn: DuckDbConnection) -> Self {
        Self { conn }
    }

    /// 插入或更新容器记录
    pub fn upsert(&self, record: &ContainerRecord) -> DuckDbResult<()> {
        let service_type_str = record.service_type.to_string();
        let created_at_str = record.created_at.to_rfc3339();
        let last_activity_str = record.last_activity.to_rfc3339();

        self.conn.with_connection(|c| {
            c.execute(
                r#"
                INSERT OR REPLACE INTO containers (
                    container_id, container_name, container_ip,
                    internal_port, external_port, service_type,
                    status, service_url, created_at, last_activity
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                params![
                    record.container_id,
                    record.container_name,
                    record.container_ip,
                    record.internal_port as i32,
                    record.external_port as i32,
                    service_type_str,
                    record.status,
                    record.service_url,
                    created_at_str,
                    last_activity_str,
                ],
            )?;
            Ok(())
        })
    }

    /// 根据容器ID查找容器
    pub fn find_by_id(&self, container_id: &str) -> DuckDbResult<Option<ContainerRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT container_id, container_name, container_ip,
                       internal_port, external_port, service_type,
                       status, service_url, created_at, last_activity
                FROM containers
                WHERE container_id = ?
                "#,
            )?;

            let mut rows = stmt.query(params![container_id])?;

            match rows.next()? {
                Some(row) => Ok(Some(Self::row_to_record(row)?)),
                None => Ok(None),
            }
        })
    }

    /// 删除容器
    pub fn delete(&self, container_id: &str) -> DuckDbResult<bool> {
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "DELETE FROM containers WHERE container_id = ?",
                params![container_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 检查容器是否存在
    pub fn exists(&self, container_id: &str) -> DuckDbResult<bool> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare("SELECT 1 FROM containers WHERE container_id = ? LIMIT 1")?;
            let mut rows = stmt.query(params![container_id])?;
            Ok(rows.next()?.is_some())
        })
    }

    /// 更新容器最后活动时间
    pub fn update_activity(&self, container_id: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "UPDATE containers SET last_activity = ? WHERE container_id = ?",
                params![now_str, container_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 根据会话ID更新关联容器的最后活动时间
    pub fn update_activity_by_session(&self, session_id: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                r#"
                UPDATE containers
                SET last_activity = ?
                WHERE container_id IN (
                    SELECT container_id FROM projects WHERE session_id = ?
                )
                "#,
                params![now_str, session_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 使用指定时间更新容器最后活动时间（用于保持项目和容器时间一致）
    pub fn update_activity_with_time(
        &self,
        container_id: &str,
        time: DateTime<Utc>,
    ) -> DuckDbResult<bool> {
        let time_str = time.to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "UPDATE containers SET last_activity = ? WHERE container_id = ?",
                params![time_str, container_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 更新容器状态
    pub fn update_status(&self, container_id: &str, status: &str) -> DuckDbResult<bool> {
        let now_str = Utc::now().to_rfc3339();
        self.conn.with_connection(|c| {
            let affected = c.execute(
                "UPDATE containers SET status = ?, last_activity = ? WHERE container_id = ?",
                params![status, now_str, container_id],
            )?;
            Ok(affected > 0)
        })
    }

    /// 获取所有容器
    pub fn find_all(&self) -> DuckDbResult<Vec<ContainerRecord>> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT container_id, container_name, container_ip,
                       internal_port, external_port, service_type,
                       status, service_url, created_at, last_activity
                FROM containers
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

    /// 按服务类型查找容器
    pub fn find_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> DuckDbResult<Vec<ContainerRecord>> {
        let service_type_str = service_type.to_string();
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(
                r#"
                SELECT container_id, container_name, container_ip,
                       internal_port, external_port, service_type,
                       status, service_url, created_at, last_activity
                FROM containers
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

    /// 查找闲置容器（超过指定分钟数未活动且不在保护期内）
    pub fn find_idle_containers(
        &self,
        idle_minutes: i64,
        protection_minutes: i64,
    ) -> DuckDbResult<Vec<IdleContainerInfo>> {
        self.conn.with_connection(|c| {
            // 使用 DuckDB 的时间函数计算
            let mut stmt = c.prepare(
                r#"
                SELECT c.container_id, c.container_name, c.service_type,
                       DATEDIFF('minute', c.last_activity, NOW()) as idle_mins
                FROM containers c
                WHERE DATEDIFF('minute', c.last_activity, NOW()) >= ?
                  AND DATEDIFF('minute', c.created_at, NOW()) >= ?
                ORDER BY idle_mins DESC
                "#,
            )?;

            let mut rows = stmt.query(params![idle_minutes, protection_minutes])?;
            let mut results = Vec::new();

            while let Some(row) = rows.next()? {
                let container_id: String = row.get(0)?;
                let container_name: String = row.get(1)?;
                let service_type_str: String = row.get(2)?;
                let idle_mins: i64 = row.get(3)?;

                let service_type = service_type_str
                    .parse::<ServiceType>()
                    .map_err(|e| DuckDbError::InternalError(format!("failed to parse service type: {}", e)))?;

                results.push(IdleContainerInfo {
                    container_id,
                    container_name,
                    service_type,
                    idle_minutes: idle_mins,
                    project_ids: Vec::new(), // 稍后填充
                });
            }

            Ok(results)
        })
    }

    /// 查找孤立容器（存在于 Docker 但不在数据库中）
    ///
    /// 此方法用于与外部容器列表进行比对
    pub fn find_orphan_containers(
        &self,
        docker_container_ids: &[String],
    ) -> DuckDbResult<Vec<String>> {
        if docker_container_ids.is_empty() {
            return Ok(Vec::new());
        }

        self.conn.with_connection(|c| {
            // 获取数据库中所有容器 ID
            let mut stmt = c.prepare("SELECT container_id FROM containers")?;
            let mut rows = stmt.query([])?;
            let mut db_container_ids = std::collections::HashSet::new();

            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                db_container_ids.insert(id);
            }

            // 找出在 Docker 中存在但不在数据库中的容器
            let orphans: Vec<String> = docker_container_ids
                .iter()
                .filter(|id| !db_container_ids.contains(*id))
                .cloned()
                .collect();

            Ok(orphans)
        })
    }

    /// 获取容器数量统计
    pub fn count(&self) -> DuckDbResult<usize> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare("SELECT COUNT(*) FROM containers")?;
            let mut rows = stmt.query([])?;
            let row = rows
                .next()?
                .ok_or_else(|| DuckDbError::InternalError("unable to get container count".to_string()))?;
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        })
    }

    /// 按服务类型统计容器数量
    pub fn count_by_service_type(
        &self,
    ) -> DuckDbResult<std::collections::HashMap<ServiceType, usize>> {
        self.conn.with_connection(|c| {
            let mut stmt =
                c.prepare("SELECT service_type, COUNT(*) FROM containers GROUP BY service_type")?;
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

    /// 从数据库行转换为 ContainerRecord
    fn row_to_record(row: &duckdb::Row<'_>) -> DuckDbResult<ContainerRecord> {
        let container_id: String = row.get(0)?;
        let container_name: String = row.get(1)?;
        let container_ip: String = row.get(2)?;
        let internal_port: i32 = row.get(3)?;
        let external_port: i32 = row.get(4)?;
        let service_type_str: String = row.get(5)?;
        let status: String = row.get(6)?;
        let service_url: String = row.get(7)?;

        // DuckDB 返回 TIMESTAMP 类型，需要使用 get_ref 并转换
        let created_at = Self::get_timestamp_from_row(row, 8)?;
        let last_activity = Self::get_timestamp_from_row(row, 9)?;

        let service_type = service_type_str
            .parse::<ServiceType>()
            .map_err(|e| DuckDbError::InternalError(format!("failed to parse service type: {}", e)))?;

        Ok(ContainerRecord {
            container_id,
            container_name,
            container_ip,
            internal_port: internal_port as u16,
            external_port: external_port as u16,
            service_type,
            status,
            service_url,
            created_at,
            last_activity,
        })
    }

    /// 从行中获取时间戳
    fn get_timestamp_from_row(row: &duckdb::Row<'_>, idx: usize) -> DuckDbResult<DateTime<Utc>> {
        // DuckDB 将 TIMESTAMP 返回为微秒级 Unix 时间戳 (i64)
        // 或者可能返回字符串，取决于存储方式
        use duckdb::types::ValueRef;

        let value_ref = row.get_ref(idx)?;
        match value_ref {
            ValueRef::Timestamp(_, micros) => {
                // 微秒转秒和纳秒
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
            _ => Ok(Utc::now()), // 默认返回当前时间
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaInitializer;

    fn setup_test_db() -> ContainerRepository {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        SchemaInitializer::initialize(&conn).unwrap();
        ContainerRepository::new(conn)
    }

    fn create_test_record(id: &str) -> ContainerRecord {
        ContainerRecord::new(
            id.to_string(),
            format!("container-{}", id),
            "127.0.0.1".to_string(),
            8080,
            8080,
            ServiceType::RCoder,
            "running".to_string(),
            format!("http://localhost:8080/{}", id),
        )
    }

    #[test]
    fn test_upsert_and_find() {
        let repo = setup_test_db();
        let record = create_test_record("c1");

        repo.upsert(&record).unwrap();

        let found = repo.find_by_id("c1").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.container_id, "c1");
        assert_eq!(found.container_name, "container-c1");
    }

    #[test]
    fn test_delete() {
        let repo = setup_test_db();
        let record = create_test_record("c1");

        repo.upsert(&record).unwrap();
        assert!(repo.exists("c1").unwrap());

        repo.delete("c1").unwrap();
        assert!(!repo.exists("c1").unwrap());
    }

    #[test]
    fn test_update_activity() {
        let repo = setup_test_db();
        let record = create_test_record("c1");

        repo.upsert(&record).unwrap();

        let before = repo.find_by_id("c1").unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        repo.update_activity("c1").unwrap();

        let after = repo.find_by_id("c1").unwrap().unwrap();
        assert!(after.last_activity >= before.last_activity);
    }

    #[test]
    fn test_count() {
        let repo = setup_test_db();

        assert_eq!(repo.count().unwrap(), 0);

        repo.upsert(&create_test_record("c1")).unwrap();
        repo.upsert(&create_test_record("c2")).unwrap();

        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn test_find_by_service_type() {
        let repo = setup_test_db();

        let mut rcoder = create_test_record("c1");
        rcoder.service_type = ServiceType::RCoder;

        let mut agent = create_test_record("c2");
        agent.service_type = ServiceType::ComputerAgentRunner;

        repo.upsert(&rcoder).unwrap();
        repo.upsert(&agent).unwrap();

        let rcoder_containers = repo.find_by_service_type(ServiceType::RCoder).unwrap();
        assert_eq!(rcoder_containers.len(), 1);
        assert_eq!(rcoder_containers[0].container_id, "c1");
    }
}

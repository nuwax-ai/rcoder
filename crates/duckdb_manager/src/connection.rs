//! DuckDB 连接管理
//!
//! 提供线程安全的数据库连接管理

use crate::error::{DuckDbError, DuckDbResult};
use duckdb::Connection;
use parking_lot::Mutex;
use std::sync::Arc;

/// 线程安全的 DuckDB 连接包装器
///
/// 使用 `Arc<Mutex<Connection>>` 实现线程安全访问
#[derive(Clone)]
pub struct DuckDbConnection {
    inner: Arc<Mutex<Connection>>,
}

impl DuckDbConnection {
    /// DuckDB 内存限制默认值（2GB）
    const DEFAULT_MEMORY_LIMIT: &str = "2GB";

    /// 创建内存模式的数据库连接
    pub fn open_in_memory() -> DuckDbResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| DuckDbError::ConnectionError(e.to_string()))?;

        // 设置 DuckDB 内存限制，防止无限占用内存
        // 注意：memory_limit 只限制 buffer manager，部分数据结构（如向量、查询结果）
        // 可能在 buffer manager 外部分配，因此实际内存可能略高于此限制
        conn.execute(
            &format!("PRAGMA memory_limit='{}'", Self::DEFAULT_MEMORY_LIMIT),
            [],
        )
        .map_err(|e| DuckDbError::ConnectionError(format!("Failed to set memory_limit: {}", e)))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// 创建内存模式的数据库连接（带自定义内存限制）
    ///
    /// # Arguments
    /// * `memory_limit` - 内存限制字符串，如 "2GB", "512MB", "1GB"
    pub fn open_in_memory_with_limit(memory_limit: &str) -> DuckDbResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| DuckDbError::ConnectionError(e.to_string()))?;

        // 设置自定义内存限制
        conn.execute(
            &format!("PRAGMA memory_limit='{}'", memory_limit),
            [],
        )
        .map_err(|e| DuckDbError::ConnectionError(format!("Failed to set memory_limit: {}", e)))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// 执行需要独占连接的操作
    ///
    /// 使用闭包模式确保锁的正确释放
    pub fn with_connection<F, T>(&self, f: F) -> DuckDbResult<T>
    where
        F: FnOnce(&Connection) -> DuckDbResult<T>,
    {
        let conn = self.inner.lock();
        f(&conn)
    }

    /// 执行需要可变连接的操作
    pub fn with_connection_mut<F, T>(&self, f: F) -> DuckDbResult<T>
    where
        F: FnOnce(&mut Connection) -> DuckDbResult<T>,
    {
        let mut conn = self.inner.lock();
        f(&mut conn)
    }

    /// 尝试获取连接（非阻塞）
    pub fn try_with_connection<F, T>(&self, f: F) -> DuckDbResult<Option<T>>
    where
        F: FnOnce(&Connection) -> DuckDbResult<T>,
    {
        match self.inner.try_lock() {
            Some(conn) => Ok(Some(f(&conn)?)),
            None => Ok(None),
        }
    }

    /// 获取 DuckDB 内存使用统计
    ///
    /// 返回格式化的内存使用信息字符串，用于调试和监控
    pub fn get_memory_stats(&self) -> DuckDbResult<String> {
        self.with_connection(|c| {
            // 查询内存使用情况
            let mut stmt = c.prepare(
                "SELECT name, size, reservation FROM duckdb_memory() ORDER BY size DESC LIMIT 10",
            )?;
            let mut rows = stmt.query([])?;

            let mut result = String::from("DuckDB Memory Usage (Top 10):\n");
            while let Some(row) = rows.next()? {
                let name: String = row.get(0).unwrap_or_default();
                let size: i64 = row.get(1).unwrap_or(0);
                let reservation: i64 = row.get(2).unwrap_or(0);
                result.push_str(&format!(
                    "  {}: size={}, reservation={}\n",
                    name, size, reservation
                ));
            }
            Ok(result)
        })
    }

    /// 执行事务
    ///
    /// 自动处理事务的提交和回滚
    pub fn transaction<F, T>(&self, f: F) -> DuckDbResult<T>
    where
        F: FnOnce(&Connection) -> DuckDbResult<T>,
    {
        let conn = self.inner.lock();

        // 开始事务
        conn.execute("BEGIN TRANSACTION", [])
            .map_err(|e| DuckDbError::TransactionError(format!("failed to begin transaction: {}", e)))?;

        // 执行操作
        match f(&conn) {
            Ok(result) => {
                // 提交事务
                conn.execute("COMMIT", [])
                    .map_err(|e| DuckDbError::TransactionError(format!("failed to commit transaction: {}", e)))?;
                Ok(result)
            }
            Err(e) => {
                // 回滚事务
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }
}

impl std::fmt::Debug for DuckDbConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDbConnection")
            .field("inner", &"<Connection>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let conn = DuckDbConnection::open_in_memory();
        assert!(conn.is_ok());
    }

    #[test]
    fn test_with_connection() {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        let result = conn.with_connection(|c| {
            c.execute("SELECT 1", [])?;
            Ok(42)
        });
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_transaction_commit() {
        let conn = DuckDbConnection::open_in_memory().unwrap();

        // 创建测试表
        conn.with_connection(|c| {
            c.execute("CREATE TABLE test (id INTEGER)", [])?;
            Ok(())
        })
        .unwrap();

        // 使用事务插入数据
        conn.transaction(|c| {
            c.execute("INSERT INTO test VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();

        // 验证数据存在
        let count: i32 = conn
            .with_connection(|c| {
                let mut stmt = c.prepare("SELECT COUNT(*) FROM test")?;
                let mut rows = stmt.query([])?;
                let row = rows.next()?.unwrap();
                Ok(row.get(0)?)
            })
            .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn test_transaction_rollback() {
        let conn = DuckDbConnection::open_in_memory().unwrap();

        // 创建测试表
        conn.with_connection(|c| {
            c.execute("CREATE TABLE test (id INTEGER)", [])?;
            Ok(())
        })
        .unwrap();

        // 使用事务插入数据，但返回错误触发回滚
        let result: DuckDbResult<()> = conn.transaction(|c| {
            c.execute("INSERT INTO test VALUES (1)", [])?;
            Err(DuckDbError::InternalError("故意触发回滚".to_string()))
        });

        assert!(result.is_err());

        // 验证数据不存在（已回滚）
        let count: i32 = conn
            .with_connection(|c| {
                let mut stmt = c.prepare("SELECT COUNT(*) FROM test")?;
                let mut rows = stmt.query([])?;
                let row = rows.next()?.unwrap();
                Ok(row.get(0)?)
            })
            .unwrap();

        assert_eq!(count, 0);
    }
}

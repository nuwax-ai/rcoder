//! Turso 后端的 SQL 执行薄层：把 sqlx 风格的调用映射到 Turso API。
//!
//! 只在 worker 上下文内使用（连接由专用线程独占）；参数用 [`Value`] 显式
//! 构造，返回值统一解包为 String/i64/Vec——保持与原 sqlx 实现相同的
//! “错误即 Err、无行即 None”语义（存储失败 ≠ 不存在）。

use turso::{Connection, Value};

use crate::userapp_lifecycle::storage;
use shared_types::UserAppStoreError as Error;

/// 文本参数（原 sqlx `.bind(String)`）。
pub(super) fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}

/// 整数参数。
pub(super) fn integer(value: i64) -> Value {
    Value::Integer(value)
}

/// 读取一行单列文本（原 `query_scalar(...).fetch_optional`）。
pub(super) async fn q_opt_string(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Option<String>, Error> {
    let mut rows = conn.query(sql, params).await.map_err(storage)?;
    match rows.next().await.map_err(storage)? {
        None => Ok(None),
        Some(row) => {
            let value = row.get_value(0).map_err(storage)?;
            match value {
                Value::Null => Ok(None),
                Value::Text(s) => Ok(Some(s)),
                other => Err(Error::InvalidOperation(format!(
                    "expected text column, got {:?}",
                    other
                ))),
            }
        }
    }
}

/// 读取一行单列整数（deadline_ms 等原生 INTEGER 列）。
pub(super) async fn q_opt_i64(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Option<i64>, Error> {
    let mut rows = conn.query(sql, params).await.map_err(storage)?;
    match rows.next().await.map_err(storage)? {
        None => Ok(None),
        Some(row) => match row.get_value(0).map_err(storage)? {
            Value::Null => Ok(None),
            Value::Integer(i) => Ok(Some(i)),
            other => Err(Error::InvalidOperation(format!(
                "expected integer column, got {:?}",
                other
            ))),
        },
    }
}

/// 读取多行单列文本（fetch_all）。
pub(super) async fn q_all_string(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<String>, Error> {
    let mut rows = conn.query(sql, params).await.map_err(storage)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage)? {
        match row.get_value(0).map_err(storage)? {
            Value::Text(s) => out.push(s),
            Value::Null => continue,
            other => {
                return Err(Error::InvalidOperation(format!(
                    "expected text column, got {:?}",
                    other
                )));
            }
        }
    }
    Ok(out)
}

/// 多行多列读取（原 `query_as` 元组）：每行按列序返回 Value。
pub(super) async fn q_rows(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut rows = conn.query(sql, params).await.map_err(storage)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage)? {
        let columns = row.column_count();
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(row.get_value(index).map_err(storage)?);
        }
        out.push(values);
    }
    Ok(out)
}

/// `BEGIN IMMEDIATE` 事务（原 sqlx `begin_with("BEGIN IMMEDIATE")`）。
///
/// Drop 回滚语义（turso 0.8.0-pre.11 源码验证，T0 探针补充）：`Transaction`
/// Drop 时在连接上记 `dangling_tx = Rollback`（src/transaction.rs:228），
/// 连接的下一次 query/execute/execute_batch/BEGIN 会先执行
/// `maybe_handle_dangling_tx` 发出 ROLLBACK 再运行语句
/// （src/connection.rs:101；`transaction_with_behavior` 在
/// src/transaction.rs:289 同样先处理）。本 worker 独占单连接且串行执行，
/// 因此 `?` 错误路径 Drop 事务 = 懒但必然的回滚——下一个方法先回滚遗留
/// 事务再执行，不毒化连接；回滚失败以存储错误在下一个方法上暴露（隔离
/// 不变）。成功响应只可能来自 [`Tx::commit`] 之后。
pub(super) struct Tx<'a> {
    tx: Option<turso::transaction::Transaction<'a>>,
}

fn tx_finished() -> Error {
    Error::InvalidOperation("transaction already finished".into())
}

pub(super) async fn begin(conn: &mut Connection) -> Result<Tx<'_>, Error> {
    let tx = conn
        .transaction_with_behavior(turso::transaction::TransactionBehavior::Immediate)
        .await
        .map_err(storage)?;
    Ok(Tx { tx: Some(tx) })
}

impl Tx<'_> {
    /// 事务内执行。返回受影响行数（Turso execute 返回 u64——
    /// rows_affected 语义已由 T0 探针验证与 UPDATE/DELETE 一致）。
    pub(super) async fn exec(&mut self, sql: &str, params: Vec<Value>) -> Result<u64, Error> {
        let conn = self.tx.as_ref().ok_or(tx_finished())?;
        conn.execute(sql, params).await.map_err(storage)
    }

    /// 事务内单列文本（fetch_optional 语义）。
    pub(super) async fn opt_string(
        &mut self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Option<String>, Error> {
        let mut rows = self.query(sql, params).await?;
        match rows.next().await.map_err(storage)? {
            None => Ok(None),
            Some(row) => match row.get_value(0).map_err(storage)? {
                Value::Null => Ok(None),
                Value::Text(s) => Ok(Some(s)),
                other => Err(Error::InvalidOperation(format!(
                    "expected text column, got {:?}",
                    other
                ))),
            },
        }
    }

    /// 事务内单列文本（fetch_one 语义；无行 = NotFound）。
    pub(super) async fn one_string(
        &mut self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<String, Error> {
        self.opt_string(sql, params).await?.ok_or(Error::NotFound)
    }

    /// 事务内多行单列文本。
    pub(super) async fn all_string(
        &mut self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<String>, Error> {
        let mut rows = self.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage)? {
            match row.get_value(0).map_err(storage)? {
                Value::Text(s) => out.push(s),
                Value::Null => continue,
                other => {
                    return Err(Error::InvalidOperation(format!(
                        "expected text column, got {:?}",
                        other
                    )));
                }
            }
        }
        Ok(out)
    }

    /// 事务内查询（Transaction Deref 到 Connection——直接 query）。
    pub(super) async fn query(
        &mut self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<turso::Rows, Error> {
        let conn = self.tx.as_ref().ok_or(tx_finished())?;
        conn.query(sql, params).await.map_err(storage)
    }

    /// 提交（成功响应只能发生在 commit 之后）。
    pub(super) async fn commit(mut self) -> Result<(), Error> {
        match self.tx.take() {
            Some(tx) => tx.commit().await.map_err(storage),
            None => Err(Error::InvalidOperation(
                "transaction already finished".into(),
            )),
        }
    }
}

/// 事务内多行多列读取（原 `query_as` 元组）。
pub(super) async fn q_rows_tx(
    tx: &mut Tx<'_>,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut rows = tx.query(sql, params).await.map_err(storage)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage)? {
        let columns = row.column_count();
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(row.get_value(index).map_err(storage)?);
        }
        out.push(values);
    }
    Ok(out)
}

/// Value → 借用侧文本（行数据解包）。
pub(super) fn as_text(value: &Value) -> Result<&str, Error> {
    match value {
        Value::Text(s) => Ok(s),
        other => Err(Error::InvalidOperation(format!(
            "expected text value, got {:?}",
            other
        ))),
    }
}

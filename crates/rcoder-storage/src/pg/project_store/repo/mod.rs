//! 主服务域的数据访问层（repo）：containers/projects/sessions 三表纯 SQL
//! （UserApp 业务表的 SQL 在 `crate::pg::userapp::repo/`,两域互不触碰）。
//!
//! 分层约定（参考 sqlx 官方 axum-social-with-tests / transaction 示例）：
//! - **本层**：纯 SQL 语句 + 行类型（FromRow），无业务逻辑、无状态；
//!   函数一律接收 `impl PgExecutor`——pool、连接、事务解引用均可执行
//!   （writer 的事务内调用传 `&mut *tx`）。
//! - **业务层**（本域的 `../writer`/`../load` 等）：编排、重试、快照构造、
//!   trait 实现——不写 SQL，只调本层。
//!
//! 语句规范：全部参数绑定（sqlx 0.9 起 `SqlSafeStr` 禁止动态拼接）；
//! 写路径幂等（upsert / delete），供 writer 整批重放。

pub(in crate::pg) mod rows;
pub(in crate::pg) mod store_repo;

pub(in crate::pg) use rows::{ContainerRow, ProjectRow, SessionRow};
pub(in crate::pg) use store_repo::*;

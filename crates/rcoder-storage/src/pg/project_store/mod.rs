//! 主服务域：`ProjectStore` 契约的 PG 后端实现（与 `pg/userapp/` 平行的业务域）。
//!
//! - `PgStore`（`crate::pg` 门面）的实现切面：`store_impl`（ProjectStore trait 实现）、
//!   `durable`（事务直写，同步路径）、`persist_ops`（write-behind op 模型）；
//! - 读写路径：`writer`（write-behind 异步批量落库）/ `load`（启动全量加载 + 回源组装）/
//!   `sync`（跨副本镜像同步）；
//! - `leader`（多副本选主，PgStore 后台机制）；
//! - `repo/`：本域纯 SQL（containers/projects/sessions 三表）——Userapp 业务表
//!   的 SQL 在 `pg/userapp/repo/`，两域互不触碰。
//!
//! 语句规范：全部参数绑定（SqlSafeStr 禁动态拼接），写路径幂等。
//! 与 `pg/userapp/` 的目录级隔离见 `crate::pg` 模块文档。

pub(crate) mod durable;
pub(crate) mod leader;
pub(crate) mod load;
pub(crate) mod persist_ops;
pub(crate) mod repo;
pub(crate) mod store_impl;
pub mod sync;
pub(crate) mod writer;

#[cfg(test)]
mod tests;

//! Userapp 业务表的数据访问（userapp_activity / userapp_metadata）。
//!
//! 分层约定与主服务 repo 一致（见 `crate::pg::repo`）：纯 SQL + 行映射,
//! 函数一律接收 `impl PgExecutor`;全部参数绑定,写路径幂等。

pub(in crate::pg) mod activity_repo;
pub(in crate::pg) mod metadata_repo;

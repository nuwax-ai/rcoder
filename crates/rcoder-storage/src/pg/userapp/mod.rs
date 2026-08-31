//! Userapp 业务的 PG 持久化域（activity / metadata 两对适配+repo 归拢于此）。
//!
//! 与主服务（ProjectStore 的 store_repo/writer/load/sync/durable/leader）物理隔离：
//! 修改 Userapp 业务表只碰本目录,不影响主服务读写路径（开闭原则的目录化）。
//!
//! - 适配层（本目录）:实现 shared_types 的持久化契约(trait),编排、无 SQL;
//! - repo 层(`repo/`):纯 SQL + 行映射(`impl PgExecutor`,pool/事务均可执行);
//! - 契约位置:activity/metadata 在 `shared_types`(跨 crate 契约,app_manager 产出/消费)。
//!
//! 语句规范与主服务一致:全部参数绑定,写路径幂等(upsert/delete)。
//! （publish_tasks 域已随 rcoder 侧 publish 任务体系删除；表与迁移保留不动。）

pub mod activity;
pub mod metadata;
pub(crate) mod repo;

#[cfg(test)]
mod tests;

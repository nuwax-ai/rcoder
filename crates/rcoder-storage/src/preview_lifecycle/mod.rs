//! Custom Page 预览生命周期存储（K8s=PG 实现；Compose=进程内实现见 preview-coordinator）。
//!
//! 契约与状态机语义见 `shared_types::preview`；本模块只做持久化与 CAS，
//! 不执行网络 I/O、不做证据核实（恢复证据由协调器核实后传入）。
mod domain;

#[cfg(feature = "pg")]
mod postgres;
#[cfg(feature = "pg")]
pub use postgres::PgPreviewStore;

#[cfg(all(test, feature = "pg"))]
mod pg_tests;

//! 编排内核域：serve 状态机（server + journal/preparation 子模块）、操作受理
//! 内核、启动/业务就绪判定、空闲常驻与编排事件协议。
pub mod business_readiness;
pub mod idle;
pub mod orchestration_events;
pub mod runtime_kernel;
pub mod runtime_status;
pub mod server;
// 根级私有模块（原 lib.rs `mod` 声明）：仅 crate 内可见。
pub(crate) mod migration_journal;
pub(crate) mod startup_probe;

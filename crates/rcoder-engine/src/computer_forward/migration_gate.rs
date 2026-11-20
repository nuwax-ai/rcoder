//! 迁移写门禁——实现已下移 `shared_types::migration_gate`（第五轮复审：
//! docker_manager 的首建/续接路径需要同一把门禁做写排空，guard 需要
//! Clone 语义跨入 file-server 的同步写线程）。此 re-export 保持 engine
//! 侧引用稳定。
pub use shared_types::migration_gate::{
    admit_write, begin, drain_user_writes, end_with_token, is_migrating, is_write_method,
    WriteAdmission, WriteAdmissionGuard,
};

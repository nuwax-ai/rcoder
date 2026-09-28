//! 跨平台运行态单一所有者抽象层（cross-platform.md §2）。
//!
//! 平台差异收敛为两个内部结构：
//! - [`owner_guard::OwnerGuard`]：本机跨进程排他锁（std 文件锁，1.89+）
//! - [`process_utils::guardian::OwnedChild`]：受管进程树（process-wrap：
//!   Unix 进程组 / Windows Job Object）
//!
//! 本模块不含 unsafe：文件锁走 std，进程树由 process_utils 统一管理。

pub mod owner_guard;
pub mod process_tree;

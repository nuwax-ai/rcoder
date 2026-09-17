//! 跨平台运行态单一所有者抽象层（cross-platform.md §2）。
//!
//! 平台差异收敛为两个内部结构：
//! - [`owner_guard::OwnerGuard`]：本机跨进程排他锁（std 文件锁，1.89+）
//! - [`process_tree::ManagedChild`]：受管进程树（command-group：
//!   Unix 进程组 / Windows Job Object，spawn 前归属无逃逸窗口）
//!
//! 本模块不含 unsafe：文件锁走 std，进程组走 command-group/watchexec。

pub mod owner_guard;
pub mod process_tree;

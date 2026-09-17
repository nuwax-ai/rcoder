//! 跨平台运行态单一所有者抽象层（cross-platform.md §2）。
//!
//! 平台差异收敛为三个内部 trait/结构：
//! - [`owner_guard::OwnerGuard`]：本机跨进程排他锁
//! - [`process_tree::ManagedProcessTree`]：进程树管理（Windows Job Objects）
//! - 文件替换与持久化：复用 [`crate::server_journal`] 的原子写
//!
//! SAFETY: 本模块使用 unsafe 仅限 FFI 调用 C/Win32 API（flock/setsid/killpg/
//! LockFileEx/Job Objects），符合 AGENTS.md §3 工程约束。

#![allow(unsafe_code)]

pub mod owner_guard;
pub mod process_tree;

//! 监督 wire 的 discovery 文件镜像（runtime-supervisor 侧为 pub(crate)，
//! 此处按 wire 格式镜像；字段名/serde 形态与 control.rs 的类型一致，
//! 两端测试锁同一格式）。
//!
//! 独立成跨平台模块而非留在 `supervision_tests`：后者因真实进程 fixture 使用
//! POSIX flock/信号/可执行脚本而整体 `cfg(all(test, unix))`，但本类型只依赖
//! `runtime_supervisor` 的 serde 形态、无平台依赖；`start::tests` 的 owner
//! 复用用例在各平台都要写同一份 fixture，随该模块门控会连带丢掉 Windows 覆盖。

use runtime_supervisor::{Request, Snapshot};

#[derive(serde::Serialize)]
pub(super) struct WireDiscoveryFile {
    pub(super) version: u32,
    pub(super) instance: String,
    pub(super) address: String,
    pub(super) token: String,
    pub(super) snapshot: Snapshot,
    pub(super) requests: Vec<(Request, Snapshot)>,
}

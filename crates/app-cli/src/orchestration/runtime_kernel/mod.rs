//! 运行态单一所有者内核（阶段二，specs/userapp-runtime-ownership §3）。
//!
//! app-cli `serve` 是唯一运行态所有者：start/restart/deploy/stop 全部经
//! [`RuntimeKernel::admit`] 受理——短锁内完成鉴权/重放/冲突/修订/恢复检查，
//! 持久化 Accepted 记录后才派发执行（复用 server 主循环这一既有串行
//! worker：制品走部署通道，源码/停止走编排通道）。操作记录与事件按
//! workspace 卷根（父目录）下的 `.app-cli-state/` 稳定状态根持久化
//! （R02：跨热部署换代稳定），进程重建后可查询、
//! 可按 operation_id + request_digest 幂等重放。
//!
//! 关键不变量（spec R05/R06/R07）：
//! - 受理先持久化；终态发布前持久化；未确认停写保持恢复保护；
//! - HTTP/SSE 观察者断开不取消执行（worker 独立持有）；
//! - 同 operation_id 同摘要返回原结果；异摘要 409；
//! - stop 是持久化意图屏障：受理即提升 revision，旧构建提交被拒。
//!
//! 拆分（file-server 大文件范式）：`store` 状态根持久化（RuntimeStore +
//! json 落盘助手）/ `kernel` 受理内核（RuntimeKernel + Admission 状态）。
//! 旧路径 `crate::runtime_kernel::X` 经 glob 重导出保持不变。

use anyhow::{Context, Result};
use shared_types::{
    DesiredState, ERR_INTERRUPTED_OWNER_EXIT, ERR_OPERATION_ID_CONFLICT, ERR_OPERATION_IN_PROGRESS,
    ERR_RECOVERY_REQUIRED, ERR_REVISION_MISMATCH, ERR_RUNTIME_INSTANCE_MISMATCH,
    ERR_WORKSPACE_MISMATCH, RUNTIME_CONTROL_PROTOCOL_VERSION, RuntimeEventRecord,
    RuntimeFailureDetail, RuntimeIdentityView, RuntimeOperationKind, RuntimeOperationRequest,
    RuntimeOperationState, RuntimeOperationView, RuntimeStatusView, runtime_request_digest,
    validate_runtime_operation_request,
};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// 稳定状态根目录名（R02/B04）。
///
/// 权威根 = env `APP_CLI_STATE_ROOT`（平台注入，source/.run/别名同一目录）；
/// 缺省 `{workspace 卷根}/.app-cli-state/{application_id}`（按应用隔离）。
/// 绝不放入会被热部署替换的 workspace 内。旧位置（in-workspace、bare 卷根）
/// 由 [`RuntimeStore::open_with_root`] 一次性迁移；新旧并存 fail-fast。
pub(crate) const STATE_DIR_NAME: &str = ".app-cli-state";

mod kernel;
mod store;
#[cfg(test)]
mod tests;

// kernel 全部条目为 pub(crate)——glob 以同档可见性重导出（pub 会触发
// "no imported item is public enough" 噪声警告）。
pub(crate) use kernel::*;
pub use store::*;

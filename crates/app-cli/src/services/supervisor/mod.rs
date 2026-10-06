//! 子项目 + pingap 编排核心：wait PG → migrate → start 子项目 → spawn pingap → supervise。
//!
//! 替代 workspace start.sh。由 main.rs 调用 `run(&args)`，前台阻塞直到任一子进程退出或收到信号，
//! 然后 kill 所有子进程 + return → supervisor [program:app] 感知退出 → 整组重启。
//!
//! 拆分（file-server 大文件范式）：`run` 入口与执行档位 / `pg_wait` PG 就绪
//! 等待 / `start` 子项目启动 / `pingap` 反代进程 / `supervise` 收束与信号。
//! 旧路径 `crate::supervisor::X` 经 glob 重导出保持不变。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::RuntimeArgs;
use crate::manifest::{self, ServiceSpec};
use crate::orchestration_events::{FailedService, OrchestrationEvent, emit as emit_event};
use crate::platform::process_tree::StopOutcome;
use crate::proxy::admin_probe;
use crate::proxy::compiler::compile_and_validate;
use crate::proxy::pingap::PINGAP_PORT;
use crate::runtime_status::RuntimeStatusService;
use process_utils::guardian::{OwnedChild as ManagedChild, spawn_owned};

/// supervisor 全部真实子进程（业务服务 / migrate / pingap）统一走受管进程树：
/// Unix 进程组 / Windows Job Object（R01——spawn 前归属无逃逸窗口，停止收束整树）。
type ManagedChildren = Vec<(String, ManagedChild)>;

/// 失败终局 Done 中编排器自身条目的 service 名（区别于用户服务；平台侧
/// failed 清单按条目映射进任务失败汇总，编排阶段错误不再依赖超时兜底）。
pub(crate) const ORCHESTRATOR_FAILURE_SERVICE: &str = "orchestrator";

mod migration;
mod pg_wait;
mod pingap;
mod run;
mod start;
mod supervise;
#[cfg(test)]
mod tests;

// 私有 glob 升 pub(crate)：拆分前的 pub(crate) 项（wait_for_service_ready_within /
// workspace_needs_pg / ShutdownUnconfirmed 等）被 xmlrpc/supervisord_host/
// startup_probe 经 `supervisor::X` 路径引用——glob 重导出按各条目自身可见性封顶，
// pub(super) 项仍限 supervisor 子树。
pub(crate) use migration::*;
pub(crate) use pg_wait::*;
use pingap::*;
pub use run::*;
pub use start::*;
pub(crate) use supervise::*;

//! 常驻 server：状态机 + 编排主循环（`app-cli serve`）。
//!
//! 形态演进：legacy 直跑（无子命令）是"一次性编排进程"——无 lock 起不来、服务
//! 崩整组重启；server 形态把 app-cli 变成**常驻管理服务**：无论是否部署都在
//! （Idle 态照常应答探针，空容器不 CrashLoop），部署/编排是状态机的一个阶段，
//! 新部署请求可打断当前编排（热切换）。
//!
//! 状态机：
//! ```text
//!  Idle ──(env APP_DEPLOY_URL | /v1/deploy)──▶ Deploying ──▶ Orchestrating ──▶ Running
//!   ▲                                                                    │
//!   └──────────────────── 新部署请求（先停旧服务）◀───────────────────────┘
//!  任一阶段失败 → Failed（/ready 503 摘流、/health 200 不杀容器，可再次部署）
//! ```
//!
//! 探针语义（kubelet 契约）：`/health` 恒 200（进程活）；`/ready` = Idle 200
//! （基础设施就绪——PG/ttyd/dbx 由 supervisord 固定 program 自治）/ Running 跟随
//! bridge readiness / 其余 503（摘流不杀）。
//!
//! 拆分（file-server 大文件范式）：`state` 全局状态与类型 / `state_impl` 状态机
//! 方法 / `serve` 入口与 attach / `startup` 启动装配与恢复 / `run_loop` 主循环；
//! `journal`、`preparation` 为既有子模块。旧路径 `crate::server::X` 经 glob
//! 重导出保持不变。

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
mod deploy_replay;
pub mod journal;
pub(crate) use deploy_replay::DeployAdmission;
mod preparation;
use journal::{ActiveVersion, Boundary, Journal, Receipt};
use shared_types::AppCliDeployPhase;
use shared_types::app_cli_deploy::AppDeploymentStage;
use tokio_util::sync::CancellationToken;

use crate::config::RuntimeArgs;
use crate::log::service::LogLayout;
use crate::manifest::ReleaseLock;
use crate::runtime_status::RuntimeStatusService;
use crate::supervisor;
use crate::supervisord_host::SupervisordHost;

#[cfg(unix)]
const DEPLOY_PROTOCOL: u32 = shared_types::app_cli_deploy::APP_CLI_UNIFIED_DEPLOY_PROTOCOL;
// Non-Unix supervision cannot yet prove descendant process-group quiescence.
#[cfg(not(unix))]
const DEPLOY_PROTOCOL: u32 = shared_types::app_cli_deploy::APP_CLI_OPERATION_ID_DEPLOY_PROTOCOL;

mod run_loop;
mod serve;
mod startup;
mod state;
mod state_impl;
#[cfg(test)]
mod tests;

// 对外 API（原 pub 项）经 glob 保持 `crate::server::X` 路径；全私有块的子模块
// 私有 glob 仅供 mod.rs 作用域（子模块经 `use super::*` 互见）。
use run_loop::*;
pub use serve::*;
use startup::*;
pub use state::*;

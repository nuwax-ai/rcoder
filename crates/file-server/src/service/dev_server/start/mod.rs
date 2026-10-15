//! dev server 启动: start_dev / start_dev_inner / poll_alive + 端口/启动守卫。
//!
//! 拆分（file-server 大文件范式）：`launch` 启动守卫与 legacy 启动管线
//! （start_dev/start_dev_inner/spawn_and_register）/ `manifest` app-cli
//! 引擎启动 / `owner` owner 探测复用与制品路由 / `alive` 就绪轮询与
//! legacy 探测 / `tests` 回归网。impl 块按方法组拆至各文件。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::error_classify::{STDERR_RING_CAP, StderrRing};
use super::log;
use super::port_pool::PortPool;
use super::process;
use super::supervise::SupervisedChild;
use super::support::{early_exit_err, ldrtemp, lock, read_dev_script};
use super::types::{AliveProbe, DevServerManager, StartedDev};
use crate::error::{AppError, AppResult};
use crate::models::{DevProcess, ExternalOwner};
use crate::service::pnpm::{self, InstallOptions, LogFiles};
use std::path::Path;

mod alive;
mod launch;
mod manifest;
mod owner;
#[cfg(test)]
mod tests;

// 拆分前 pub(super) = dev_server 层可见；拆深一层后 StartingGuard
// （coordinated.rs 经 `start::StartingGuard` 构造守卫）与
// legacy_app_cli_responds（mod.rs/owner_client.rs 经 `start::` 路径引用）
// 升 pub(crate)，glob 重导出按各条目自身可见性封顶。
pub(crate) use alive::*;
pub(crate) use launch::*;

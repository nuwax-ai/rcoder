//! Durable external-owner intents. File transactions are short and never span HTTP.
//!
//! 拆分（file-server 大文件范式）：`state` 持久化状态结构（Intent/
//! OwnerRecord/OwnerIdentity/State + 状态 IO 助手）/ `intent` manager 侧
//! 事务与 intent 生命周期（prepare/resume/finish + verify_view）/
//! `recover` 对外查询与显式恢复入口 / `tests` 回归网。
//! 旧路径 `dev_server::external_store::{State,OwnerRecord,OwnerIdentity,
//! verify_view}` 经 glob 重导出保持不变。

use std::{collections::HashMap, fs::OpenOptions, io::Write, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_types::{RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationView};

use super::{owner_client::OwnerClient, types::DevServerManager};
use crate::models::ExternalOwner;

mod intent;
mod recover;
mod state;
#[cfg(test)]
mod tests;

// 拆分前 pub(super) = dev_server 层可见（types.rs/stop.rs/owner_recovery.rs
// 等兄弟模块经 `external_store::X` 路径引用）；拆深一层后统一升 pub(crate)，
// glob 重导出按各条目自身可见性封顶，external_store 模块本身仍私有。
pub(crate) use intent::*;
pub(crate) use state::*;

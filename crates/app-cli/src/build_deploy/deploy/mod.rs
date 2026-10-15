//! URL 制品部署段（生产 Userapp 运行容器，RBD 卷形态）。
//!
//! rcoder `start {url}` 经 Deployment env 注入三元组：`APP_DEPLOY_URL`（制品 zip 地址）、
//! `APP_RELEASE_ID`（部署身份标识）、`APP_DEPLOY_SHA256`（可选校验，空 = 信任内网源）。
//! prepare downloads and validates into operation-owned temporary resources while
//! the old application continues serving. activate promotes prepared code only after
//! the caller stops the old processes. Failed promotion repairs directory consistency;
//! business recovery requires an explicit deployment and never reverses migrations. RAII removes temporary
//! resources on completion, error, and cancellation, including blocking extraction.
//!
//! 拆分（file-server 大文件范式）：`env` env 三元组声明与部署请求判定 /
//! `liveness` 部署期探针端口托管（已退役，保留设计参考）/ `pipeline` 两阶段
//! 部署（prepare 下载校验 .incoming → activate 提升 .previous 回滚恢复）/
//! `download` 制品下载（预算内重试 + zip 魔数校验）/ `extract` 制品解压。
//! 旧路径 `crate::deploy::X` 经 glob/显式重导出保持不变。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_types::AppDeploymentProgress;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

/// 进度回调类型（prepare 内部按阶段调用，外部按需消费）。
pub(crate) type ProgressCallback = Arc<dyn Fn(AppDeploymentProgress) + Send + Sync + 'static>;

/// 部署状态 marker 文件名（卷根下，跨 code 换代存续）。
const DEPLOY_STATE_FILE: &str = ".deploy-state.toml";
/// 下载中转目录名（卷根下）。
const INCOMING_DIR: &str = ".incoming";
/// 解压 staging 目录名（卷根下）。
const STAGING_DIR: &str = ".staging";
/// 上一代 code 保留目录名（卷根下，仅一代）。
const PREVIOUS_DIR: &str = ".previous";

/// 部署状态（幂等 marker）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeployState {
    release_id: String,
    /// 实际下载内容的 sha256（hex，空 sha 部署时也有值——下载总会计算）。
    sha256: String,
    deployed_at: String,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

mod download;
mod env;
mod extract;
mod liveness;
mod pipeline;
#[cfg(test)]
mod tests;

// 对外条目（原 pub/pub(crate) 项）经 glob/显式重导出保持 `crate::deploy::X`
// 路径；`download` 协作项经私有 use 仅供子模块 `use super::*` 互见。
use download::{download_to_file, verify_zip_magic};
pub use env::*;
pub(crate) use extract::*;
pub use liveness::*;
#[cfg(test)]
use pipeline::prepare;
pub(crate) use pipeline::{
    PreparedDeploy,
    activate,
    cleanup_startup,
    deploy,
    prepare_with_local,
    // tests 经 `use super::*` 消费 prepare（非测试构建无消费者，单独门控避免 unused 告警）。
    restore_previous_generation,
};

// 测试独用：块内私有助手升 pub(super)，经 cfg(test) 私有导入供 tests 取用。
#[cfg(test)]
use pipeline::{
    PreparationLease, incoming_dir, read_state, sweep_incoming_parts, validate_release_id_fs_safe,
    volume_root_of,
};

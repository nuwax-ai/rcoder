//! Userapp Manifest v1 shared contract.
//!
//! Build-time (`file-server`) and runtime (`app-cli`) both consume this crate。
//!
//! # 配置演进策略
//!
//! 平台会持续演进（镜像升级、新增字段等）。为避免"前期考虑不足、后期无法升级"，
//! schema 变更按下列两类处理：
//!
//! - **加性变更**（新增可选字段）：用 `Option` + `#[serde(default)]`，**不动
//!   [`SCHEMA_VERSION`]**。新 reader 读老配置时字段缺失自动填默认；老 reader 读新配置
//!   的场景被 release lock 的 `minimum_app_cli_version` 门禁挡掉。**零迁移代码**。
//!   范例：`ReleaseLock.bridge_service`。
//!
//! - **破坏性变更**（重命名/删除/改类型/改推导）：**bump [`SCHEMA_VERSION`]**，并：
//!   1. 把当前 `ReleaseLock` 形状冻结成 `legacy::v{N}::LegacyV{N}Lock`（各 struct 照贴
//!      `deny_unknown_fields`，保 fail-fast）；
//!   2. `ReleaseLock` 升为新当前型；
//!   3. 写 `migrate_v{N}_v{N+1}` 迁移函数；
//!   4. 在 [`load_release_lock`] 的 `match` 加历史版本分支；
//!   5. golden 测试加 v{N}→v{N+1} 快照（`tests/fixtures/lock_v{N}.toml` 永久保留）。
//!
//!   破坏性变更再分两种失败模式：
//!   - 可仅从老 lock 推导 → 读时迁移（迁移函数返回 `Ok`）；
//!   - 需源 manifest 重推导 → 返回 [`LoadError::RequiresRebuild`]，平台侧用 zip 内
//!     manifest 重锁（`build_release_lock` 是纯幂等函数，端口/拓扑序是 manifest 的
//!     确定性纯函数；zip 内含原始 manifest，故重锁无需用户源码）。
//!
//! **纪律：破坏性变更 PR 必须同时含迁移函数 + golden 测试；无迁移不合入。**
//! 详见 [`load_release_lock`] 文档中的"引入 v2"步骤。

mod discovery;
mod error;
mod release_lock;
mod types;
mod validation;

pub use discovery::{assemble_discovered, discover_projects, discover_projects_lenient};
pub use error::{DiscoverError, LoadError, ManifestError};
pub use release_lock::{ReleaseMetadata, build_release_lock, load_release_lock};
pub use types::*;
pub use validation::{
    ValidationIssue, collect_topology_issues, collect_workspace_issues, manifest_file_of,
    parse_project, parse_project_toml, parse_workspace, validate_project, validate_project_at,
    validate_service_id, validate_topology, validate_workspace,
};

pub const SCHEMA_VERSION: u32 = 1;
pub const INTERNAL_PORT_MIN: u16 = 4000;
pub const INTERNAL_PORT_MAX: u16 = 7999;

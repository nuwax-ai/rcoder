//! Agent-store: 智能体级实体存储 (对齐 TS `agentStoreUtils.js` + `AgentWorkspaceUtils.js`)。
//!
//! 目录结构: `{COMPUTER_WORKSPACE_DIR}/{userId}/.agent-store/{agentId}/{skills,agents}/`
//! 与会话工作区 `{COMPUTER_WORKSPACE_DIR}/{userId}/{cId}` 同属一棵树。
//!
//! 核心能力:
//! - 跨平台目录链接 (`force_dir_symlink`) — Unix 相对软链 / Windows junction
//! - 工作区软链 (`link_workspace_to_agent_store`) — 软链优先, 失败 fallback copy
//! - 技能安装/覆盖 (`install_skill_dir`) — 逐个子目录原子覆盖, 天然并发安全
//! - agents 更新 (`update_agents_dir`) — 逐个子目录并发覆盖, 无锁安全
//! - 差集清理 (`prune_agent_skills`, 保留 `.dynamic_add.lock` 的)
//! - 按需安装判断 (`agent_skill_exists`)
//!
//! **无锁设计**: 所有写操作都是"逐个子目录: 删旧 → rename 移入"的原子操作。
//! 不同子目录天然无冲突; 同名子目录并发覆盖最终一致。不需要文件锁。
//!
//! 拆分（file-server 大文件范式）：`store` 实体存储（安装/prune/软链/copy
//! 兜底）/ `view` 共享技能视图（manifest 并集 + 项目级视图锁）/ `tests`
//! 回归网。旧路径 `crate::service::agent_store::X` 经 glob 重导出保持不变；
//! 跨块引用与测试所需的私有项以 pub(super) 限 agent_store 子树。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use futures_util::future::try_join_all;
use tokio::fs;

use crate::error::AppResult;

mod store;
#[cfg(test)]
mod tests;
mod view;

pub use store::*;
pub use view::*;

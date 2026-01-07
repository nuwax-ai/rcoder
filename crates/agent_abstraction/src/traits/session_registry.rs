//! # Session Registry Trait
//!
//! 定义会话注册表的抽象接口，允许 `SacpSessionManager` 使用不同的存储实现。
//! 通过依赖注入避免 `agent_abstraction` 和 `agent_runner` 之间的循环依赖。
//!
//! ## 预期实现
//!
//! 此 trait 预期在 `agent_runner` 中实现：
//!
//! - **实现类**: `agent_runner::service::AgentSessionRegistry`
//! - **全局单例**: `agent_runner::service::AGENT_REGISTRY`
//!
//! ## 架构说明
//!
//! ```text
//! agent_abstraction                    agent_runner
//! ┌─────────────────┐                 ┌─────────────────────┐
//! │ SessionRegistry │◄────────────────│ AgentSessionRegistry│
//! │ (trait)         │   implements    │ (struct)            │
//! └────────┬────────┘                 └──────────┬──────────┘
//!          │                                     │
//!          │                                     │
//! ┌────────▼────────┐                 ┌──────────▼──────────┐
//! │SacpSessionManager◄────────────────│ AGENT_REGISTRY      │
//! │ registry: R     │   injects       │ (static LazyLock)   │
//! └─────────────────┘                 └─────────────────────┘
//! ```
//!
//! ## 与 AGENT_REGISTRY 的关系
//!
//! - **AGENT_REGISTRY**: `agent_runner` 中的全局单例，实现了此 trait
//! - **何时直接访问 AGENT_REGISTRY**: 在 `agent_runner` 内部进行状态查询、清理任务
//! - **何时通过 trait 访问**: 在 `agent_abstraction` 内部，通过泛型参数 `R: SessionRegistry`
//!
//! ## 使用示例
//!
//! ```ignore
//! // 在 agent_runner 中定义实现
//! pub struct AgentSessionRegistry {
//!     agent_info_map: DashMap<String, ProjectAndAgentInfo>,
//!     project_to_session: DashMap<String, String>,
//!     session_to_project: DashMap<String, String>,
//! }
//!
//! impl SessionRegistry for AgentSessionRegistry {
//!     type Entry = ProjectAndAgentInfo;
//!     // ...
//! }
//!
//! // 创建全局单例
//! pub static AGENT_REGISTRY: LazyLock<Arc<AgentSessionRegistry>> = ...;
//!
//! // 注入到 SacpSessionManager
//! let session_manager = SacpSessionManager::new(notifier, AGENT_REGISTRY.clone());
//! ```

use std::sync::Arc;

use shared_types::SessionEntry;

/// 会话注册表 trait
///
/// 抽象 session 存储的 CRUD 操作，允许不同的实现（如 AGENT_REGISTRY）。
///
/// # 设计说明
/// - `SessionEntry` trait 定义在 `shared_types`，描述单个会话条目的数据访问
/// - `SessionRegistry` trait 定义在 `agent_abstraction`，描述会话存储的 CRUD 操作
/// - `agent_runner` 为 `AGENT_REGISTRY` 实现 `SessionRegistry`，并注入到 `SacpSessionManager`
///
/// # 使用示例
/// ```ignore
/// // 定义在 agent_runner
/// impl SessionRegistry for AgentSessionRegistry {
///     type Entry = ProjectAndAgentInfo;
///     // ...
/// }
///
/// // 注入到 SacpSessionManager
/// let session_manager = SacpSessionManager::new(notifier, Arc::new(registry));
/// ```
pub trait SessionRegistry: Send + Sync + 'static {
    /// 会话条目类型
    type Entry: SessionEntry;

    /// 获取会话
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    ///
    /// # Returns
    /// 如果存在则返回会话条目的克隆，否则返回 None
    fn get(&self, project_id: &str) -> Option<Self::Entry>;

    /// 插入或更新会话
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    /// * `session_id` - 会话 ID
    /// * `entry` - 会话条目
    fn insert(&self, project_id: &str, session_id: &str, entry: Self::Entry);

    /// 移除会话
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    ///
    /// # Returns
    /// 如果存在则返回被移除的会话条目，否则返回 None
    fn remove(&self, project_id: &str) -> Option<Self::Entry>;

    /// 检查会话是否存在
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    fn contains(&self, project_id: &str) -> bool;

    /// 获取所有项目 ID 列表
    fn list_project_ids(&self) -> Vec<String>;

    /// 获取会话数量
    fn count(&self) -> usize;
}

/// SessionRegistry 的 Arc 包装器实现
///
/// 允许 `Arc<R>` 作为 `SessionRegistry` 使用
impl<R: SessionRegistry> SessionRegistry for Arc<R> {
    type Entry = R::Entry;

    fn get(&self, project_id: &str) -> Option<Self::Entry> {
        (**self).get(project_id)
    }

    fn insert(&self, project_id: &str, session_id: &str, entry: Self::Entry) {
        (**self).insert(project_id, session_id, entry)
    }

    fn remove(&self, project_id: &str) -> Option<Self::Entry> {
        (**self).remove(project_id)
    }

    fn contains(&self, project_id: &str) -> bool {
        (**self).contains(project_id)
    }

    fn list_project_ids(&self) -> Vec<String> {
        (**self).list_project_ids()
    }

    fn count(&self) -> usize {
        (**self).count()
    }
}

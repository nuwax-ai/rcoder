//! Session Registry Trait
//!
//! 定义会话注册表的抽象接口，允许 AcpSessionManager 使用不同的存储实现。
//! 通过依赖注入避免 agent_abstraction 和 agent_runner 之间的循环依赖。

use std::sync::Arc;

use shared_types::SessionEntry;

/// 会话注册表 trait
///
/// 抽象 session 存储的 CRUD 操作，允许不同的实现（如 AGENT_REGISTRY）。
///
/// # 设计说明
/// - `SessionEntry` trait 定义在 `shared_types`，描述单个会话条目的数据访问
/// - `SessionRegistry` trait 定义在 `agent_abstraction`，描述会话存储的 CRUD 操作
/// - `agent_runner` 为 `AGENT_REGISTRY` 实现 `SessionRegistry`，并注入到 `AcpSessionManager`
///
/// # 使用示例
/// ```ignore
/// // 定义在 agent_runner
/// impl SessionRegistry for AgentSessionRegistry {
///     type Entry = ProjectAndAgentInfo;
///     // ...
/// }
///
/// // 注入到 AcpSessionManager
/// let session_manager = AcpSessionManager::new(notifier, Arc::new(registry));
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

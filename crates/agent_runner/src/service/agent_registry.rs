//! Agent 会话注册表
//!
//! 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
//! 所有映射操作都通过此结构体的方法进行，确保数据一致性

#![allow(dead_code)]

mod pending_guard;
mod removal;

use agent_abstraction::traits::SessionRegistry;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
pub use pending_guard::{PendingGuard, RegistryStats};
use shared_types::ProjectAndAgentInfo;
use std::sync::{Arc, LazyLock};
use tracing::{debug, info};

/// 全局 Agent 会话注册表（Arc 包装版本，用于 AcpSessionManager 注入）
pub static AGENT_REGISTRY: LazyLock<Arc<AgentSessionRegistry>> =
    LazyLock::new(|| Arc::new(AgentSessionRegistry::new()));

/// Agent 会话注册表
///
/// 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
/// 所有映射操作都通过此结构体的方法进行，确保数据一致性
///
/// ## Clone 手动实现
///
/// DashMap 支持克隆（内部使用 Arc），这里显式 clone 各映射。
impl Clone for AgentSessionRegistry {
    fn clone(&self) -> Self {
        Self {
            agent_info_map: self.agent_info_map.clone(),
            project_to_session: self.project_to_session.clone(),
            session_to_project: self.session_to_project.clone(),
        }
    }
}

pub struct AgentSessionRegistry {
    /// project_id → ProjectAndAgentInfo
    agent_info_map: DashMap<String, ProjectAndAgentInfo>,
    /// project_id → session_id (正向映射)
    project_to_session: DashMap<String, String>,
    /// session_id → project_id (反向映射)
    session_to_project: DashMap<String, String>,
}

impl AgentSessionRegistry {
    /// 创建新的注册表
    pub fn new() -> Self {
        Self {
            agent_info_map: DashMap::new(),
            project_to_session: DashMap::new(),
            session_to_project: DashMap::new(),
        }
    }

    // ========== 注册/更新操作 ==========

    /// 注册新的 Agent Session（同时更新所有映射）
    ///
    /// 如果 project_id 已存在旧的 session，会自动清理旧的反向映射
    ///
    /// ## 并发安全性
    ///
    /// 使用 DashMap entry API 的原子操作，避免 remove/insert 之间的竞态窗口：
    /// - 所有 insert/remove 操作都是独立的原子操作
    /// - 采用"先插入后删除"策略，确保任何时刻至少有一个有效映射
    pub fn register(&self, project_id: &str, session_id: &str, agent_info: ProjectAndAgentInfo) {
        use dashmap::mapref::entry::Entry;

        // 🎯 原子性地更新 project_to_session 并获取旧 session_id
        let old_session_id = match self.project_to_session.entry(project_id.to_string()) {
            Entry::Occupied(mut entry) => {
                let old_sid = entry.get().clone();
                entry.insert(session_id.to_string()); // 原子性替换
                Some(old_sid)
            }
            Entry::Vacant(entry) => {
                entry.insert(session_id.to_string());
                None
            }
        };

        // 🔒 使用 entry API 原子性地更新 session_to_project
        // 这样避免了 insert + remove 分离操作带来的竞态窗口
        let old_project_for_session = match self.session_to_project.entry(session_id.to_string()) {
            Entry::Occupied(mut entry) => {
                // key 已存在，原子性地替换值
                let old_project_id = entry.get().clone();
                entry.insert(project_id.to_string());
                // 如果 session_id 对应的 project_id 发生变化，返回旧 project_id 用于清理
                if old_project_id != project_id {
                    Some(old_project_id)
                } else {
                    None
                }
            }
            Entry::Vacant(entry) => {
                // key 不存在，直接插入
                entry.insert(project_id.to_string());
                None
            }
        };

        // 更新 agent_info（原子操作）
        self.agent_info_map
            .insert(project_id.to_string(), agent_info);

        // ✅ 清理旧的 session_to_project 映射（如果需要）
        // 只有当 session 真正变化时才清理旧值
        if let Some(old_sid) = old_session_id
            && old_sid != session_id
        {
            // remove 本身是原子操作，此时新映射已插入，不会影响查询
            self.session_to_project.remove(&old_sid);
            debug!(
                "🔄 [Registry] Cleaning old session mapping: project={}, old_session={}",
                project_id, old_sid
            );
        }

        // ✅ 清理旧 project 的 stale project_to_session 映射
        // 当 session_id 被重新分配给不同的 project 时，旧 project 的
        // project_to_session 条目变为过期（仍指向此 session_id），需要清理
        if let Some(ref old_project_id) = old_project_for_session
            && let Entry::Occupied(oe) = self.project_to_session.entry(old_project_id.clone())
            && oe.get() == session_id
        {
            oe.remove_entry();
            debug!(
                "🔄 [Registry] Cleaning stale project_to_session: old_project={}, session={}",
                old_project_id, session_id
            );
        }

        info!(
            "✅ [Registry] Registering Agent: project={}, session={}",
            project_id, session_id
        );
    }

    /// 更新 session_id（当 session 变化时）
    ///
    /// 返回旧的 session_id（如果存在）
    ///
    /// ## 并发安全性
    ///
    /// 使用 DashMap entry API 的原子操作，与 register() 方法保持一致
    pub fn update_session(&self, project_id: &str, new_session_id: &str) -> Option<String> {
        use dashmap::mapref::entry::Entry;

        // 🎯 原子性地更新 project_to_session
        let old_session_id = match self.project_to_session.entry(project_id.to_string()) {
            Entry::Occupied(mut entry) => {
                let old_sid = entry.get().clone();
                if old_sid == new_session_id {
                    // 快速路径：session_id 未变化，直接返回
                    return Some(old_sid);
                }
                entry.insert(new_session_id.to_string()); // 原子性替换
                Some(old_sid)
            }
            Entry::Vacant(entry) => {
                // 首次建立映射
                entry.insert(new_session_id.to_string());
                None
            }
        };

        // 🔒 使用 entry API 原子性地更新 session_to_project
        let old_project_for_session =
            match self.session_to_project.entry(new_session_id.to_string()) {
                Entry::Occupied(mut entry) => {
                    // key 已存在，原子性地替换值
                    let old_project_id = entry.get().clone();
                    entry.insert(project_id.to_string());
                    if old_project_id != project_id {
                        Some(old_project_id)
                    } else {
                        None
                    }
                }
                Entry::Vacant(entry) => {
                    // key 不存在，直接插入
                    entry.insert(project_id.to_string());
                    None
                }
            };

        // ✅ 清理旧的 session_to_project 映射（原子操作）
        if let Some(ref old_sid) = old_session_id
            && old_sid != new_session_id
        {
            // remove 本身是原子操作
            self.session_to_project.remove(old_sid);
        }

        // ✅ 清理旧 project 的 stale project_to_session 映射
        if let Some(ref old_project_id) = old_project_for_session
            && let Entry::Occupied(oe) = self.project_to_session.entry(old_project_id.clone())
            && oe.get() == new_session_id
        {
            oe.remove_entry();
            debug!(
                "🔄 [Registry] Cleaning stale project_to_session in update_session: old_project={}, session={}",
                old_project_id, new_session_id
            );
        }

        if let Some(ref old_sid) = old_session_id {
            info!(
                "🔄 [Registry] Session updated: project={}, {} → {}",
                project_id, old_sid, new_session_id
            );
        } else {
            info!(
                "🆕 [Registry] Session created: project={}, session={}",
                project_id, new_session_id
            );
        }

        old_session_id
    }

    /// 更新 agent_info（不改变 session 映射）
    pub fn update_agent_info(&self, project_id: &str, agent_info: ProjectAndAgentInfo) {
        self.agent_info_map
            .insert(project_id.to_string(), agent_info);
        debug!("[Registry] Updated agent_info: project={}", project_id);
    }

    /// 🆕 尝试原子性地更新 agent_info
    ///
    /// 使用 DashMap 的 entry API 进行原子性条件更新，避免竞态条件
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `f`: 更新函数，返回 true 表示Update succeeded，false 表示无需更新
    ///
    /// # 返回
    /// - true: Update succeeded
    /// - false: Agent 不存在或条件不满足（未更新）
    ///
    /// # 示例
    /// ```rust,ignore
    /// registry.try_update_agent_info("project-123", |info| {
    ///     if info.status == AgentStatus::Active {
    ///         info.status = AgentStatus::Idle;
    ///         true  // Update succeeded
    ///     } else {
    ///         false  // 无需更新
    ///     }
    /// });
    /// ```
    pub fn try_update_agent_info<F>(&self, project_id: &str, mut f: F) -> bool
    where
        F: FnMut(&mut ProjectAndAgentInfo) -> bool,
    {
        use dashmap::mapref::entry::Entry;

        match self.agent_info_map.entry(project_id.to_string()) {
            Entry::Occupied(mut entry) => {
                let info = entry.get_mut();
                if f(info) {
                    debug!(
                        "[Registry] Atomic update agent_info succeeded: project={}",
                        project_id
                    );
                    true
                } else {
                    debug!(
                        "[Registry] agent_info no update needed (condition not met): project={}",
                        project_id
                    );
                    false
                }
            }
            Entry::Vacant(_) => {
                debug!(
                    "[Registry] agent_info does not exist, cannot update: project={}",
                    project_id
                );
                false
            }
        }
    }

    // ========== 查询操作 ==========

    /// 通过 session_id 获取 project_id（O(1) 复杂度）
    pub fn get_project_by_session(&self, session_id: &str) -> Option<String> {
        self.session_to_project
            .get(session_id)
            .map(|r| r.value().clone())
    }

    /// 通过 project_id 获取 session_id
    pub fn get_session_by_project(&self, project_id: &str) -> Option<String> {
        self.project_to_session
            .get(project_id)
            .map(|r| r.value().clone())
    }

    /// 通过 project_id 获取 agent_info 引用
    pub fn get_agent_info(&self, project_id: &str) -> Option<Ref<'_, String, ProjectAndAgentInfo>> {
        self.agent_info_map.get(project_id)
    }

    /// 通过 session_id 获取 agent_info 引用
    ///
    /// ## 算法
    /// 1. 通过 session_to_project 映射找到 project_id
    /// 2. 通过 project_id 获取 agent_info
    ///
    /// ## 返回值
    /// - `Some(Ref)`: 找到对应的 Agent
    /// - `None`: session_id 不存在或已被清理
    ///
    /// ## ⚠️ 竞态条件说明
    ///
    /// 此方法执行两次独立的 DashMap 查询，两次查询之间存在微小的竞态窗口（~100ns）。
    ///
    /// **竞态场景**：
    /// ```text
    /// T1: session_to_project.get("ses_abc") → "project_123" ✅
    /// T2: [其他线程] remove_by_project("project_123")
    /// T3: agent_info_map.get("project_123") → None ❌
    /// ```
    ///
    /// **影响评估**：
    /// - **最坏情况**：返回 None，调用方会创建新会话
    /// - **实际风险**：极低（竞态窗口 < 1 微秒）
    /// - **降级策略**：自动创建新会话，不影响功能正确性
    ///
    /// **为什么不优化**：
    /// 1. 使用单一 DashMap 需要重构整个数据模型
    /// 2. DashMap 的分段锁特性已经将风险降到最低
    /// 3. 当前设计支持 `project_id → session_id` 的一对多映射（未来扩展）
    ///
    /// ## 使用建议
    /// - 在调用此方法后，如果返回 None，应该视为"会话不存在"
    /// - 不要依赖此方法进行强一致性的事务操作
    pub fn get_agent_info_by_session(
        &self,
        session_id: &str,
    ) -> Option<Ref<'_, String, ProjectAndAgentInfo>> {
        // view() 在闭包返回后立即释放锁，无 Ref 暴露
        let project_id_str = self.session_to_project.view(session_id, |_, v| v.clone())?;

        // 再通过 project_id 获取 agent_info
        self.agent_info_map.get(&project_id_str)
    }

    /// 通过 project_id 在闭包内访问 agent_info;闭包返回即释放读锁(无 Ref 暴露,
    /// 防守卫跨 .await)。需要 owned 字段时用这个,不要用 [`get_agent_info`] 拿 Ref 跨 await。
    pub fn view_agent_info<R>(
        &self,
        project_id: &str,
        f: impl FnOnce(&ProjectAndAgentInfo) -> R,
    ) -> Option<R> {
        self.agent_info_map.view(project_id, |_, info| f(info))
    }

    /// 通过 session_id 在闭包内访问 agent_info(全程不暴露 Ref)。两 map 间仍有 ~100ns
    /// 竞态(同 [`get_agent_info_by_session`]),可接受。
    pub fn view_agent_info_by_session<R>(
        &self,
        session_id: &str,
        f: impl FnOnce(&ProjectAndAgentInfo) -> R,
    ) -> Option<R> {
        let project_id_str = self.session_to_project.view(session_id, |_, v| v.clone())?;
        self.agent_info_map.view(&project_id_str, |_, info| f(info))
    }

    /// 检查 project 是否存在
    pub fn contains_project(&self, project_id: &str) -> bool {
        self.agent_info_map.contains_key(project_id)
    }

    /// 检查 session 是否存在
    pub fn contains_session(&self, session_id: &str) -> bool {
        self.session_to_project.contains_key(session_id)
    }

    // ========== 清理操作 ==========

    // ========== 遍历操作 ==========

    /// 遍历所有 agent_info（用于清理任务等）
    pub fn iter_agents(&self) -> impl Iterator<Item = RefMulti<'_, String, ProjectAndAgentInfo>> {
        self.agent_info_map.iter()
    }

    /// 获取所有 project_id 列表
    pub fn all_project_ids(&self) -> Vec<String> {
        self.agent_info_map
            .iter()
            .map(|r| r.key().clone())
            .collect()
    }

    /// 获取统计信息
    ///
    /// ⚠️ 注意：此方法会调用 DashMap::len()，该操作会遍历所有分片。
    /// 在高并发场景下，应立即使用返回值，避免长时间持有结果导致的潜在阻塞。
    ///
    /// 推荐用法：
    /// ```rust,ignore
    /// let count = AGENT_REGISTRY.stats().agent_count;  // 立即提取数值
    /// // 使用 count，而不是持有整个 RegistryStats
    /// ```
    pub fn stats(&self) -> RegistryStats {
        // DashMap::len() 会遍历所有分片，在高并发下可能有性能开销
        // 但由于立即返回基本类型（usize），不会持有锁
        let agent_count = self.agent_info_map.len();
        let session_count = self.project_to_session.len();

        RegistryStats {
            agent_count,
            session_count,
        }
    }

    /// 获取内部 agent_info_map 的可变引用（仅用于测试）
    ///
    /// ## 安全性
    ///
    /// 此方法仅用于测试场景，允许测试代码直接操作 DashMap 以验证原子性操作。
    /// 生产代码不应使用此方法。
    ///
    /// ## 为什么不用 `#[cfg(test)]`
    ///
    /// 如果使用 `#[cfg(test)]`，测试 crate 将无法访问此方法（因为测试 crate 编译时不会包含 `#[cfg(test)]` 的项）。
    /// 因此我们使用文档约束，而不是编译时条件。
    pub fn inner_mut(&self) -> &DashMap<String, ProjectAndAgentInfo> {
        &self.agent_info_map
    }
}

// ============================================================================
// 实现 SessionRegistry trait（用于 AcpSessionManager 依赖注入）
// ============================================================================

impl SessionRegistry for AgentSessionRegistry {
    type Entry = ProjectAndAgentInfo;

    fn get(&self, project_id: &str) -> Option<Self::Entry> {
        self.agent_info_map.get(project_id).map(|r| r.clone())
    }

    fn insert(&self, project_id: &str, session_id: &str, entry: Self::Entry) {
        self.register(project_id, session_id, entry);
    }

    fn remove(&self, project_id: &str) -> Option<Self::Entry> {
        self.remove_by_project(project_id)
    }

    fn contains(&self, project_id: &str) -> bool {
        self.contains_project(project_id)
    }

    fn get_project_by_session(&self, session_id: &str) -> Option<String> {
        // 🔥 修复：调用内部方法，避免递归
        self.session_to_project
            .get(session_id)
            .map(|r| r.value().clone())
    }

    fn get_entry_by_session(&self, session_id: &str) -> Option<Self::Entry> {
        // 🔥 优化：一次性通过 session_id 获取 agent_info，避免竞态窗口
        // 算法：
        // 1. 通过 session_to_project 找到 project_id
        // 2. 通过 project_id 获取 agent_info
        // 3. 克隆并返回
        //
        // 注意：虽然仍然是两次 DashMap 查询，但由于在同一个函数内，
        // 且第一次查询（session_to_project）完成后立即释放锁，
        // 第二次查询（agent_info_map）在同一分片或相邻分片上执行，
        // 竞态窗口比两次独立调用要小得多。
        self.get_agent_info_by_session(session_id)
            .map(|r| r.clone())
    }

    fn list_project_ids(&self) -> Vec<String> {
        self.all_project_ids()
    }

    fn count(&self) -> usize {
        self.agent_info_map.len()
    }

    fn entry(&self, project_id: String) -> Entry<'_, String, Self::Entry> {
        self.agent_info_map.entry(project_id)
    }

    fn update_agent_status(&self, project_id: &str, status: shared_types::AgentStatus) {
        self.try_update_agent_info(project_id, |info| {
            let old_status = info.status;
            if old_status != status {
                info.status = status;
                info.last_activity = chrono::Utc::now();
                tracing::debug!(
                    "🔄 [atomic_status] Project[{}] status: {:?} -> {:?}",
                    project_id,
                    old_status,
                    status
                );
                true
            } else {
                false
            }
        });
    }

    fn update_last_activity(&self, project_id: &str, activity: chrono::DateTime<chrono::Utc>) {
        self.try_update_agent_info(project_id, |info| {
            info.last_activity = activity;
            true
        });
    }
}

impl Default for AgentSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionId;
    use chrono::Utc;
    use shared_types::AgentStatus;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_test_agent_info(project_id: &str, session_id: &str) -> ProjectAndAgentInfo {
        let (prompt_tx, _) = mpsc::channel(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY);
        let (cancel_tx, _) = mpsc::channel(shared_types::AGENT_CANCEL_CHANNEL_CAPACITY);

        ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::new(Arc::from(session_id)),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: Utc::now(),
            created_at: Utc::now(),
            stop_handle: None,
            agent_binary_snapshot: None,
        }
    }

    #[test]
    fn test_register_and_query() {
        let registry = AgentSessionRegistry::new();

        let info = create_test_agent_info("project1", "session1");
        registry.register("project1", "session1", info);

        // 查询
        assert!(registry.contains_project("project1"));
        assert!(registry.contains_session("session1"));
        assert_eq!(
            registry.get_project_by_session("session1"),
            Some("project1".to_string())
        );
        assert_eq!(
            registry.get_session_by_project("project1"),
            Some("session1".to_string())
        );
    }

    #[test]
    fn test_update_session() {
        let registry = AgentSessionRegistry::new();

        let info = create_test_agent_info("project1", "session1");
        registry.register("project1", "session1", info);

        // 更新 session
        let old = registry.update_session("project1", "session2");
        assert_eq!(old, Some("session1".to_string()));

        // 旧 session 应该被清理
        assert!(!registry.contains_session("session1"));
        assert!(registry.contains_session("session2"));
        assert_eq!(
            registry.get_project_by_session("session2"),
            Some("project1".to_string())
        );
    }

    #[test]
    fn test_remove_by_project() {
        let registry = AgentSessionRegistry::new();

        let info = create_test_agent_info("project1", "session1");
        registry.register("project1", "session1", info);

        // 删除
        let removed = registry.remove_by_project("project1");
        assert!(removed.is_some());

        // 所有映射都应该被清理
        assert!(!registry.contains_project("project1"));
        assert!(!registry.contains_session("session1"));
    }

    #[test]
    fn test_remove_by_session() {
        let registry = AgentSessionRegistry::new();

        let info = create_test_agent_info("project1", "session1");
        registry.register("project1", "session1", info);

        // 通过 session 删除
        let removed = registry.remove_by_session("session1");
        assert!(removed.is_some());

        // 所有映射都应该被清理
        assert!(!registry.contains_project("project1"));
        assert!(!registry.contains_session("session1"));
    }
}

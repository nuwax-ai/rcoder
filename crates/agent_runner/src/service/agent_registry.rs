//! Agent 会话注册表
//!
//! 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
//! 所有映射操作都通过此结构体的方法进行，确保数据一致性

use agent_abstraction::traits::SessionRegistry;
use dashmap::DashMap;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
use shared_types::ProjectAndAgentInfo;
use std::sync::{Arc, LazyLock};
use tracing::{debug, info};

/// 全局 Agent 会话注册表（Arc 包装版本，用于 SacpSessionManager 注入）
pub static AGENT_REGISTRY: LazyLock<Arc<AgentSessionRegistry>> =
    LazyLock::new(|| Arc::new(AgentSessionRegistry::new()));

/// 注册表统计信息
#[derive(Debug, Clone)]
pub struct RegistryStats {
    pub agent_count: usize,
    pub session_count: usize,
}

/// Agent 会话注册表
///
/// 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
/// 所有映射操作都通过此结构体的方法进行，确保数据一致性
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
        let should_clean_old = match self.session_to_project.entry(session_id.to_string()) {
            Entry::Occupied(mut entry) => {
                // key 已存在，原子性地替换值
                let old_project_id = entry.get().clone();
                entry.insert(project_id.to_string());
                // 如果 session_id 对应的 project_id 发生变化，需要清理旧映射
                Some(old_project_id != project_id)
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
        if let Some(old_sid) = old_session_id {
            if old_sid != session_id {
                // remove 本身是原子操作，此时新映射已插入，不会影响查询
                self.session_to_project.remove(&old_sid);
                debug!(
                    "🔄 [Registry] 清理旧 session 映射: project={}, old_session={}",
                    project_id, old_sid
                );
            }
        }

        info!(
            "✅ [Registry] 注册 Agent: project={}, session={}",
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
        let _should_clean_old = match self.session_to_project.entry(new_session_id.to_string()) {
            Entry::Occupied(mut entry) => {
                // key 已存在，原子性地替换值
                let old_project_id = entry.get().clone();
                entry.insert(project_id.to_string());
                Some(old_project_id != project_id)
            }
            Entry::Vacant(entry) => {
                // key 不存在，直接插入
                entry.insert(project_id.to_string());
                None
            }
        };

        // ✅ 清理旧的 session_to_project 映射（原子操作）
        if let Some(ref old_sid) = old_session_id {
            if old_sid != new_session_id {
                // remove 本身是原子操作
                self.session_to_project.remove(old_sid);
            }
        }

        if let Some(ref old_sid) = old_session_id {
            info!(
                "🔄 [Registry] Session 更新: project={}, {} → {}",
                project_id, old_sid, new_session_id
            );
        } else {
            info!(
                "🆕 [Registry] Session 新建: project={}, session={}",
                project_id, new_session_id
            );
        }

        old_session_id
    }

    /// 更新 agent_info（不改变 session 映射）
    pub fn update_agent_info(&self, project_id: &str, agent_info: ProjectAndAgentInfo) {
        self.agent_info_map
            .insert(project_id.to_string(), agent_info);
        debug!("[Registry] 更新 agent_info: project={}", project_id);
    }

    /// 设置项目为 Pending 状态（用于预占位，防止并发请求）
    ///
    /// 如果项目不存在，则创建一个占位记录
    /// 如果项目已存在且为 Idle 状态，则更新为 Pending
    pub fn set_pending(&self, project_id: &str) {
        use sacp::schema::SessionId;
        use chrono::Utc;
        use shared_types::AgentStatus;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        match self.agent_info_map.entry(project_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                // 已存在：仅当 Idle 时更新为 Pending
                let info = entry.get_mut();
                if info.status == AgentStatus::Idle {
                    info.status = AgentStatus::Pending;
                    info.last_activity = Utc::now();
                    debug!("📌 [Registry] 项目 {} 状态: Idle → Pending", project_id);
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // 不存在：创建占位记录
                let (prompt_tx, _) = mpsc::unbounded_channel();
                let (cancel_tx, _) = mpsc::unbounded_channel();

                let placeholder = ProjectAndAgentInfo {
                    project_id: project_id.to_string(),
                    session_id: SessionId::new(Arc::from("pending")),
                    prompt_tx,
                    cancel_tx,
                    model_provider: None,
                    request_id: None,
                    status: AgentStatus::Pending,
                    last_activity: Utc::now(),
                    created_at: Utc::now(),
                    stop_handle: None,
                };

                entry.insert(placeholder);
                info!("📌 [Registry] 创建 Pending 占位: project_id={}", project_id);
            }
        }
    }

    /// 清理 Pending 状态（仅当当前状态为 Pending 时移除）
    ///
    /// 用于在任务失败时清理预占位，避免死锁
    pub fn clear_pending_if_exists(&self, project_id: &str) {
        use shared_types::AgentStatus;

        if let Some(info) = self.agent_info_map.get(project_id) {
            if info.status == AgentStatus::Pending {
                drop(info);
                self.remove_by_project(project_id);
                info!("🗑️ [Registry] 清理 Pending 占位: project_id={}", project_id);
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
    pub fn get_agent_info(&self, project_id: &str) -> Option<Ref<String, ProjectAndAgentInfo>> {
        self.agent_info_map.get(project_id)
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

    /// 通过 project_id 移除所有相关映射
    ///
    /// 返回被移除的 ProjectAndAgentInfo（如果存在）
    pub fn remove_by_project(&self, project_id: &str) -> Option<ProjectAndAgentInfo> {
        use dashmap::mapref::entry::Entry;

        info!(
            "🔍 [Registry] remove_by_project 开始: project_id={}",
            project_id
        );

        // 🎯 原子性地移除 project_to_session 并获取 session_id
        info!("🔍 [Registry] 移除 project_to_session 映射");
        let session_id = match self.project_to_session.entry(project_id.to_string()) {
            Entry::Occupied(entry) => {
                let (_, session_id) = entry.remove_entry(); // 原子性移除
                Some(session_id)
            }
            Entry::Vacant(_) => None,
        };
        info!("🔍 [Registry] project_to_session 移除完成");

        // 移除反向映射
        if let Some(ref sid) = session_id {
            info!("🔍 [Registry] 移除 session_to_project 映射");
            self.session_to_project.remove(sid);
            info!("🔍 [Registry] session_to_project 移除完成");
        }

        // 移除 agent_info
        info!(
            "🔍 [Registry] 准备移除 agent_info_map, project_id={}, 当前 map 长度={}",
            project_id,
            self.agent_info_map.len()
        );

        // 检查 key 是否存在
        let key_exists = self.agent_info_map.contains_key(project_id);
        info!(
            "🔍 [Registry] agent_info_map key 存在检查: project_id={}, exists={}",
            project_id, key_exists
        );

        // 执行 remove 操作
        info!("🔍 [Registry] 开始执行 agent_info_map.remove()...");
        let removed = self.agent_info_map.remove(project_id).map(|(_, v)| v);
        info!(
            "🔍 [Registry] agent_info_map.remove() 完成, removed={}, 剩余长度={}",
            removed.is_some(),
            self.agent_info_map.len()
        );

        if removed.is_some() {
            info!(
                "🗑️ [Registry] 移除 Agent: project={}, session={:?}",
                project_id, session_id
            );
        }

        info!(
            "🔍 [Registry] remove_by_project 完成: project_id={}",
            project_id
        );
        removed
    }

    /// 通过 session_id 移除所有相关映射
    ///
    /// 返回被移除的 ProjectAndAgentInfo（如果存在）
    pub fn remove_by_session(&self, session_id: &str) -> Option<ProjectAndAgentInfo> {
        use dashmap::mapref::entry::Entry;

        // 🎯 原子性地移除 session_to_project 并获取 project_id
        let project_id = match self.session_to_project.entry(session_id.to_string()) {
            Entry::Occupied(entry) => {
                let (_, project_id) = entry.remove_entry(); // 原子性移除
                Some(project_id)
            }
            Entry::Vacant(_) => None,
        };

        // 如果找到 project_id，移除正向映射和 agent_info
        if let Some(ref pid) = project_id {
            self.project_to_session.remove(pid);
            let removed = self.agent_info_map.remove(pid).map(|(_, v)| v);

            if removed.is_some() {
                info!(
                    "🗑️ [Registry] 通过 session 移除 Agent: session={}, project={}",
                    session_id, pid
                );
            }

            return removed;
        }

        None
    }

    // ========== 遍历操作 ==========

    /// 遍历所有 agent_info（用于清理任务等）
    pub fn iter_agents(&self) -> impl Iterator<Item = RefMulti<String, ProjectAndAgentInfo>> {
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
    pub fn stats(&self) -> RegistryStats {
        RegistryStats {
            agent_count: self.agent_info_map.len(),
            session_count: self.project_to_session.len(),
        }
    }
}

// ============================================================================
// 实现 SessionRegistry trait（用于 SacpSessionManager 依赖注入）
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

    fn list_project_ids(&self) -> Vec<String> {
        self.all_project_ids()
    }

    fn count(&self) -> usize {
        self.agent_info_map.len()
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
    use sacp::schema::SessionId;
    use chrono::Utc;
    use shared_types::AgentStatus;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_test_agent_info(project_id: &str, session_id: &str) -> ProjectAndAgentInfo {
        let (prompt_tx, _) = mpsc::unbounded_channel();
        let (cancel_tx, _) = mpsc::unbounded_channel();

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

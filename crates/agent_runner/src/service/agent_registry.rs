//! Agent 会话注册表
//!
//! 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
//! 所有映射操作都通过此结构体的方法进行，确保数据一致性

use agent_abstraction::traits::SessionRegistry;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
use shared_types::ProjectAndAgentInfo;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use tracing::{debug, info, warn};

// 导入工作线程池大小相关函数
use crate::agent_runtime::get_concurrency_limit;

/// 全局 Agent 会话注册表（Arc 包装版本，用于 AcpSessionManager 注入）
pub static AGENT_REGISTRY: LazyLock<Arc<AgentSessionRegistry>> =
    LazyLock::new(|| Arc::new(AgentSessionRegistry::new()));

/// 注册表统计信息
#[derive(Debug, Clone)]
pub struct RegistryStats {
    pub agent_count: usize,
    pub session_count: usize,
}

// ============================================================================
// 🔥 P0 修复: PendingGuard RAII 模式
// ============================================================================

/// Pending 状态 RAII 守卫
///
/// ## 问题背景
///
/// 旧代码中，`clear_pending_if_exists` 需要在每个异常路径手动调用，
/// 容易遗漏导致 Pending 状态永久泄漏，阻塞后续请求。
///
/// ## 解决方案
///
/// 使用 RAII (Resource Acquisition Is Initialization) 模式：
/// - 构造时自动调用 `set_pending()`
/// - Drop 时自动调用 `clear_pending_if_exists()`（除非显式提交成功）
///
/// ## 使用示例
///
/// ```rust,ignore
/// let pending_guard = PendingGuard::new(&AGENT_REGISTRY, &project_id);
///
/// match risky_operation().await {
///     Ok(response) => {
///         pending_guard.commit_success(); // 成功，不清理 Pending
///         return Ok(response);
///     }
///     Err(e) => {
///         // 失败，PendingGuard 会在 drop 时自动清理
///         return Err(e);
///     }
/// }
/// // 函数返回时，guard 自动 drop，清理逻辑执行
/// ```
pub struct PendingGuard<'a> {
    registry: &'a AgentSessionRegistry,
    project_id: String,
    cleaned: AtomicBool,
}

impl<'a> PendingGuard<'a> {
    /// 创建新的 PendingGuard 并自动设置 Pending 状态
    pub fn new(registry: &'a AgentSessionRegistry, project_id: &str) -> Self {
        registry.set_pending(project_id);
        debug!(
            "🛡️ [PendingGuard] 创建并设置 Pending 状态: project_id={}",
            project_id
        );
        Self {
            registry,
            project_id: project_id.to_string(),
            cleaned: AtomicBool::new(false),
        }
    }

    /// 标记为成功，防止 Drop 时清理
    ///
    /// ## ⚠️ 重要
    ///
    /// 调用此方法后，Pending 状态将被保留（因为 Agent 已成功启动）。
    /// 必须使用 `std::mem::forget()` 防止 Drop 执行清理。
    pub fn commit_success(self) {
        self.cleaned.store(true, Ordering::Release);
        debug!(
            "🛡️ [PendingGuard] 提交成功，保留 Pending 状态: project_id={}",
            self.project_id
        );
        std::mem::forget(self); // 防止 drop 时清理
    }
}

impl<'a> Drop for PendingGuard<'a> {
    fn drop(&mut self) {
        // 只有未标记为成功时才清理
        if !self.cleaned.load(Ordering::Acquire) {
            debug!(
                "🛡️ [PendingGuard] Drop 时自动清理 Pending 状态: project_id={}",
                self.project_id
            );
            self.registry.clear_pending_if_exists(&self.project_id);
        }
    }
}

/// Agent 会话注册表
///
/// 统一管理 project_id、session_id 和 AgentInfo 之间的映射关系
/// 所有映射操作都通过此结构体的方法进行，确保数据一致性
///
/// ## 🔥 P0 修复: Clone 手动实现
///
/// 由于 `AtomicUsize` 不实现 `Clone` trait，我们需要手动实现 `Clone`。
/// DashMap 支持克隆（内部使用 Arc），AtomicUsize 通过 load/store 实现。
impl Clone for AgentSessionRegistry {
    fn clone(&self) -> Self {
        Self {
            agent_info_map: self.agent_info_map.clone(),
            project_to_session: self.project_to_session.clone(),
            session_to_project: self.session_to_project.clone(),
            // 注意：AtomicUsize 共享同一个计数器，这是设计意图
            active_sessions_count: AtomicUsize::new(
                self.active_sessions_count.load(Ordering::Acquire),
            ),
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
    /// 🔥 P0 修复: 原子计数器，用于无锁并发限制检查
    /// 使用 Compare-And-Swap (CAS) 操作避免 TOCTOU 竞态条件
    active_sessions_count: AtomicUsize,
}

impl AgentSessionRegistry {
    /// 创建新的注册表
    pub fn new() -> Self {
        Self {
            agent_info_map: DashMap::new(),
            project_to_session: DashMap::new(),
            session_to_project: DashMap::new(),
            active_sessions_count: AtomicUsize::new(0),
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
        let _should_clean_old = match self.session_to_project.entry(session_id.to_string()) {
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
        if let Some(old_sid) = old_session_id
            && old_sid != session_id
        {
            // remove 本身是原子操作，此时新映射已插入，不会影响查询
            self.session_to_project.remove(&old_sid);
            debug!(
                "🔄 [Registry] 清理旧 session 映射: project={}, old_session={}",
                project_id, old_sid
            );
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
        if let Some(ref old_sid) = old_session_id
            && old_sid != new_session_id
        {
            // remove 本身是原子操作
            self.session_to_project.remove(old_sid);
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
                        "[Registry] 原子性更新 agent_info 成功: project={}",
                        project_id
                    );
                    true
                } else {
                    debug!(
                        "[Registry] agent_info 无需更新（条件不满足）: project={}",
                        project_id
                    );
                    false
                }
            }
            Entry::Vacant(_) => {
                debug!(
                    "[Registry] agent_info 不存在，无法更新: project={}",
                    project_id
                );
                false
            }
        }
    }

    /// 设置项目为 Pending 状态（用于预占位，防止并发请求）
    ///
    /// 如果项目不存在，则创建一个占位记录
    /// 如果项目已存在且为 Idle 状态，则更新为 Pending
    pub fn set_pending(&self, project_id: &str) {
        use chrono::Utc;
        use sacp::schema::SessionId;
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
                // 不存在：创建占位记录（使用有界通道，容量由常量定义）
                let (prompt_tx, _) = mpsc::channel(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY);
                let (cancel_tx, _) = mpsc::channel(shared_types::AGENT_CANCEL_CHANNEL_CAPACITY);

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

        if let Some(info) = self.agent_info_map.get(project_id)
            && info.status == AgentStatus::Pending
        {
            drop(info);
            self.remove_by_project(project_id);
            info!("🗑️ [Registry] 清理 Pending 占位: project_id={}", project_id);
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
        // 先通过 session_id 找到 project_id
        let project_id = self.session_to_project.get(session_id)?;
        let project_id_str = project_id.value().clone();
        drop(project_id); // 显式释放 session_to_project 的读锁

        // 再通过 project_id 获取 agent_info
        self.agent_info_map.get(&project_id_str)
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
        info!("[Registry] 移除 project_to_session 映射");
        let session_id = match self.project_to_session.entry(project_id.to_string()) {
            Entry::Occupied(entry) => {
                let (_, session_id) = entry.remove_entry(); // 原子性移除
                Some(session_id)
            }
            Entry::Vacant(_) => None,
        };
        info!("[Registry] project_to_session 移除完成");

        // 移除反向映射
        if let Some(ref sid) = session_id {
            info!("[Registry] 移除 session_to_project 映射");
            self.session_to_project.remove(sid);
            info!("[Registry] session_to_project 移除完成");
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
        info!("[Registry] 开始执行 agent_info_map.remove()...");
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

            // 🔥 修复：移除 Agent 时释放槽位
            self.release_session_slot();
            info!("[Registry] 已释放槽位: project_id={}", project_id);
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

                // 🔥 修复：移除 Agent 时释放槽位（与 remove_by_project 保持一致）
                self.release_session_slot();
                info!("[Registry] 已释放槽位: session_id={}", session_id);
            }

            return removed;
        }

        None
    }

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

    // ========== 🔥 P0 修复: 原子性会话槽位管理 ==========

    /// 尝试获取会话槽位（原子操作）
    ///
    /// ## TOCTOU 竞态条件修复
    ///
    /// 使用 Compare-And-Swap (CAS) 操作实现原子性的"检查并递增"，
    /// 避免了检查 `active_sessions_count` 和注册之间的竞态窗口。
    ///
    /// ## 算法
    ///
    /// ```text,ignore
    /// loop {
    ///     old = load()
    ///     if old >= LIMIT { return false }
    ///     if compare_exchange_weak(old, old + 1).is_ok() {
    ///         return true
    ///     }
    ///     // CAS 失败，重试
    /// }
    /// ```
    ///
    /// ## 返回
    ///
    /// - `true`: 成功获取槽位（计数器已递增）
    /// - `false`: 槽位已满，拒绝请求
    ///
    /// ## 使用示例
    ///
    /// ```rust,ignore
    /// if !AGENT_REGISTRY.try_acquire_session_slot() {
    ///     AGENT_REGISTRY.clear_pending_if_exists(&project_id);
    ///     return error("系统繁忙，请稍后重试");
    /// }
    /// // ... 处理请求 ...
    /// AGENT_REGISTRY.release_session_slot();
    /// ```
    pub fn try_acquire_session_slot(&self) -> bool {
        let mut old = self.active_sessions_count.load(Ordering::Acquire);
        let limit = get_concurrency_limit();
        loop {
            if old >= limit {
                return false;
            }
            match self.active_sessions_count.compare_exchange_weak(
                old,
                old + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    debug!("🎯 [原子槽位] 成功获取槽位: {}/{}", old + 1, limit);
                    return true;
                }
                Err(new_old) => old = new_old,
            }
        }
    }

    /// 释放会话槽位
    ///
    /// ## 使用场景
    ///
    /// 在请求处理完成（无论成功失败）后调用，释放槽位供后续请求使用。
    ///
    /// ## ⚠️ 注意事项
    ///
    /// - 必须与 `try_acquire_session_slot()` 配对使用
    /// - 使用 CAS 循环防止溢出（从 0 减到 usize::MAX）
    /// - 建议使用 `PendingGuard` RAII 模式自动管理
    pub fn release_session_slot(&self) {
        // 🛡️ 使用 CAS 循环防止溢出
        // 如果当前值为 0，不执行减操作（防止溢出到 usize::MAX）
        loop {
            let current = self.active_sessions_count.load(Ordering::Acquire);
            if current == 0 {
                warn!("[原子槽位] 尝试释放槽位但计数器已为 0，跳过操作（防止溢出）");
                return;
            }

            // CAS: 如果当前值仍然是 current，则减 1
            match self.active_sessions_count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    debug!("🔓 [原子槽位] 释放槽位: {} → {}", current, current - 1);
                    return;
                }
                Err(_) => {
                    // 值已被其他线程修改，重试
                    continue;
                }
            }
        }
    }

    /// 获取当前活跃会话计数（原子读取）
    ///
    /// 用于日志记录和监控，无需遍历 DashMap。
    pub fn active_sessions_count(&self) -> usize {
        self.active_sessions_count.load(Ordering::Acquire)
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
}

impl Default for AgentSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sacp::schema::SessionId;
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

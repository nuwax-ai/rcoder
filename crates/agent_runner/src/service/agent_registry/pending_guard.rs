//! Pending 注册 RAII 守卫与注册表统计（从 agent_registry.rs 拆出）。
//!
//! PendingGuard 是公共 API（chat_handler 集成测试以 'static 生命周期跨文件持有，
//! 签名/可见性不可动）；single-flight commit/回滚语义见 Drop。

use tracing::debug;

use super::super::agent_registry::{AgentSessionRegistry, ProjectAndAgentInfo};

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
    /// 标记是否已显式提交成功（commit_success 被调用）
    committed: bool,
}

impl<'a> PendingGuard<'a> {
    /// 创建新的 PendingGuard 并自动设置 Pending 状态
    pub fn new(registry: &'a AgentSessionRegistry, project_id: &str) -> Self {
        registry.set_pending(project_id);
        debug!(
            "🛡️ [PendingGuard] Created and set Pending status: project_id={}",
            project_id
        );
        Self {
            registry,
            project_id: project_id.to_string(),
            committed: false,
        }
    }

    /// 标记为成功，防止 Drop 时清理
    ///
    /// ## ⚠️ 重要
    ///
    /// 调用此方法后，Pending 状态将被保留（因为 Agent 已成功启动）。
    /// `committed` 标志会阻止 Drop 执行清理逻辑。
    pub fn commit_success(mut self) {
        self.committed = true;
        debug!(
            "🛡️ [PendingGuard] Commit success, keeping Pending state: project_id={}",
            self.project_id
        );
        // drop 正常运行，但 Drop::drop 检查 committed 标志后跳过清理
    }
}

impl<'a> Drop for PendingGuard<'a> {
    fn drop(&mut self) {
        // 只有未提交成功时才清理
        if !self.committed {
            debug!(
                "🛡️ [PendingGuard] Auto clearing Pending state on Drop: project_id={}",
                self.project_id
            );
            self.registry.clear_pending_if_exists(&self.project_id);
        }
    }
}

/// 注册表统计信息
#[derive(Debug, Clone)]
pub struct RegistryStats {
    pub agent_count: usize,
    pub session_count: usize,
}

// ============================================================================
// 🔥 P0 修复: PendingGuard RAII 模式
// ============================================================================

use tracing::info;

impl AgentSessionRegistry {
    /// 设置项目为 Pending 状态（用于预占位，防止并发请求）
    ///
    /// 如果项目不存在，则创建一个占位记录
    /// 如果项目已存在且为 Idle 状态，则更新为 Pending
    pub fn set_pending(&self, project_id: &str) {
        use agent_client_protocol::schema::v1::SessionId;
        use chrono::Utc;
        use dashmap::mapref::entry::Entry;
        use shared_types::AgentStatus;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        match self.agent_info_map.entry(project_id.to_string()) {
            Entry::Occupied(mut entry) => {
                // 已存在：仅当 Idle 时更新为 Pending
                let info = entry.get_mut();
                if info.status == AgentStatus::Idle {
                    info.status = AgentStatus::Pending;
                    info.last_activity = Utc::now();
                    debug!(
                        "📌 [Registry] Project {} state: Idle -> Pending",
                        project_id
                    );
                }
            }
            Entry::Vacant(entry) => {
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
                    agent_binary_snapshot: None,
                };

                entry.insert(placeholder);
                info!(
                    "📌 [Registry] Created Pending placeholder: project_id={}",
                    project_id
                );
            }
        }
    }

    /// 清理 Pending 状态（仅当当前状态为 Pending 时移除）
    ///
    /// 用于在任务失败时清理预占位，避免死锁。
    ///
    /// ## 并发安全性
    ///
    /// 使用 DashMap entry API 实现原子性的"检查状态 + 移除/回退"，
    /// 避免 clear_pending_if_exists 和 register 之间的 TOCTOU 竞态条件。
    ///
    /// ## 行为
    ///
    /// - **新建占位符**（session_id == "pending"）：完全移除条目及相关映射
    /// - **已有条目回退**（session_id != "pending"）：仅将状态从 Pending 恢复为 Idle
    pub fn clear_pending_if_exists(&self, project_id: &str) {
        use dashmap::mapref::entry::Entry;
        use shared_types::AgentStatus;

        match self.agent_info_map.entry(project_id.to_string()) {
            Entry::Occupied(mut entry) if entry.get().status == AgentStatus::Pending => {
                // 区分两种场景：
                // 1. session_id == "pending" → set_pending 创建的占位符，需完全移除
                // 2. session_id != "pending" → 从 Idle 改为 Pending 的已有条目，只需回退状态
                if entry.get().session_id.to_string() == "pending" {
                    let (_, _removed) = entry.remove_entry();
                    // 使用 entry API 安全移除 project_to_session 映射
                    // 仅当映射仍指向 "pending" 时才移除，避免与 register() 的竞态条件
                    if let Entry::Occupied(oe) =
                        self.project_to_session.entry(project_id.to_string())
                        && oe.get().as_str() == "pending"
                    {
                        oe.remove_entry();
                    }
                    info!(
                        "🗑️ [Registry] Cleared Pending placeholder: project_id={}",
                        project_id
                    );
                } else {
                    // 回退状态到 Idle，保留已有的 session 映射
                    entry.get_mut().status = AgentStatus::Idle;
                    info!(
                        "↩️ [Registry] Reverted Pending to Idle: project_id={}",
                        project_id
                    );
                }
            }
            _ => {
                // 不存在或状态不是 Pending，不操作
            }
        }
    }
}

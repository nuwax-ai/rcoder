//! Pending 注册 RAII 守卫与注册表统计（从 agent_registry.rs 拆出）。
//!
//! PendingGuard 是公共 API（chat_handler 集成测试以 'static 生命周期跨文件持有，
//! 签名/可见性不可动）；single-flight commit/回滚语义见 Drop。

use tracing::debug;

use super::super::agent_registry::AgentSessionRegistry;

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

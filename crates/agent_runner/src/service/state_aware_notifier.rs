//! StateAwareNotifier 实现
//!
//! 状态感知的 SessionNotifier 包装器，在推送 SSE 消息的同时同步更新 Agent 状态。
//!
//! ## 核心职责
//! 1. 委托给 SseSessionNotifier 推送 SSE 消息
//! 2. 通过 SessionRegistry trait 同步更新 Agent 状态（R-1 重构：不再直接依赖 AGENT_REGISTRY）
//! 3. 保持状态转换的原子性和一致性

use agent_abstraction::SessionNotifier;
use agent_abstraction::traits::SessionRegistry;
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use super::session_notifier::SseSessionNotifier;
use shared_types::log_brief::brief_log;
use shared_types::{AgentStatus, SessionNotify};

/// 默认慢轮阈值（秒）：轮次耗时超过该值时 [ROUND] 汇总升级为 WARN。
/// 后续如需可配置，通过 [`StateAwareNotifier::with_slow_round_threshold_secs`] 接线 config。
const DEFAULT_SLOW_ROUND_THRESHOLD_SECS: u64 = 120;

/// [TOOL] 骨架行里工具标题的截断长度（按字符数，中文安全）
const TOOL_TITLE_MAX_CHARS: usize = 80;

/// 轮次指标的 key：(project, session, request)。
///
/// 只用 session_id 不够：PromptEnd 在 `await` 内部 notifier 之后才移除指标，
/// 这期间同一 session 可能已开启下一轮（新 request）并插入新指标——
/// 旧轮的结束事件会**取走新轮的指标**，把新轮的耗时/计数当成旧轮的打出来，
/// 新轮自己的汇总则永久丢失。带上 request 身份后，结束事件只能移除自己的那条。
type RoundKey = (String, String, String);

/// 把 request_id 归一化成 key 分量：`None` 时退化为空串（调用方未提供身份）。
fn round_key_part(request_id: Option<&str>) -> String {
    request_id.unwrap_or_default().to_owned()
}

/// 单轮 prompt 的计时聚合指标。
///
/// ACP 语义下一 session 同时只有一个活跃 prompt（串行），
/// 因此按 (project, session, request) 建 key：PromptStart 覆盖插入，
/// PromptEnd/PromptError 按同一 key 移除。异常路径残留（start 后无 end/error）
/// 会被同 session 下一轮 PromptStart 覆盖，量级上界 = 异常 session 数，
/// 无需额外清理任务。
#[derive(Debug, Clone)]
struct RoundMetrics {
    start: Instant,
    /// 首个 AgentMessageChunk 时刻 → LLM 首 token 延迟代理指标（对所有 acp agent 通用）
    first_chunk_at: Option<Instant>,
    /// 首个 ToolCall 时刻
    first_tool_at: Option<Instant>,
    /// chunk 事件计数（只计数不打内容，避免高频日志洪流）
    chunks: u64,
    /// 工具事件计数（ToolCall + ToolCallUpdate）
    tool_events: u64,
}

impl RoundMetrics {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            first_chunk_at: None,
            first_tool_at: None,
            chunks: 0,
            tool_events: 0,
        }
    }
}

/// 状态感知的 SessionNotifier 包装器
///
/// 通过委托模式包装 SseSessionNotifier，在推送 SSE 消息的同时通过
/// `SessionRegistry` trait 同步更新 Agent 状态。
///
/// # 设计特点
/// - **依赖反转**：通过 `R: SessionRegistry` trait 引用更新状态，不直接依赖具体全局变量
/// - **委托模式**：所有 SSE 推送操作委托给内部的 SseSessionNotifier
/// - **原子性状态更新**：通过 registry trait 确保状态更新的原子性
/// - **状态同步顺序**：
///   - PromptStart: 先更新状态为 Active，再推送 SSE
///   - PromptEnd: 先推送 SSE，再恢复状态为 Idle
///   - PromptError: 先推送错误消息，再恢复状态为 Idle
#[derive(Debug, Clone)]
pub struct StateAwareNotifier<R: SessionRegistry> {
    /// 内部 SSE 推送器
    inner: Arc<SseSessionNotifier>,
    /// 通过 trait 引用操作注册表，不直接依赖 AGENT_REGISTRY
    registry: Arc<R>,
    /// 每会话当前轮次的计时聚合指标（纯观测，不参与任何业务判断）。
    /// key 为 (project, session, request)：结束事件按同一 key 移除，
    /// 不会取走同 session 下一轮（新 request）的指标。
    /// 仅单 key get_mut/insert/remove 短临界区，无嵌套锁、无跨锁调用。
    round_metrics: DashMap<RoundKey, RoundMetrics>,
    /// 慢轮判定阈值，超过则 [ROUND] 汇总行升级为 WARN
    slow_round_threshold: Duration,
}

impl<R: SessionRegistry> StateAwareNotifier<R> {
    /// 创建新的 StateAwareNotifier 实例
    ///
    /// # Arguments
    /// * `registry` - SessionRegistry trait 引用，用于更新 agent 状态
    pub fn new(registry: Arc<R>) -> Self {
        Self::with_slow_round_threshold_secs(registry, DEFAULT_SLOW_ROUND_THRESHOLD_SECS)
    }

    /// 以自定义慢轮阈值创建实例（预留 config 接线，当前调用方使用 [`Self::new`]）
    pub fn with_slow_round_threshold_secs(registry: Arc<R>, threshold_secs: u64) -> Self {
        Self {
            inner: Arc::new(SseSessionNotifier::new()),
            registry,
            round_metrics: DashMap::new(),
            slow_round_threshold: Duration::from_secs(threshold_secs),
        }
    }

    /// 更新 Agent 状态（通过 SessionRegistry trait）
    ///
    /// 使用 trait 方法实现状态更新，避免直接依赖具体的全局变量。
    /// 实际的状态更新逻辑由 `SessionRegistry` 实现方（如 `AgentSessionRegistry`）提供。
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `status`: 新的 Agent 状态
    fn update_agent_status(
        &self,
        project_id: &str,
        session_id: &str,
        request_id: Option<&str>,
        status: AgentStatus,
    ) {
        self.registry
            .update_agent_status_for_session(project_id, session_id, request_id, status);
        debug!(
            "🔄 [StateAwareNotifier] Requested status update for Project[{}] to {:?}",
            project_id, status
        );
    }
}

#[async_trait]
impl<R: SessionRegistry + 'static> SessionNotifier for StateAwareNotifier<R> {
    /// 推送会话开始通知
    ///
    /// 顺序：
    /// 1. 更新 Agent 状态为 Active
    /// 2. 推送 SessionPromptStart 到 SSE
    async fn notify_prompt_start(
        &self,
        project_id: &str,
        session_id: &str,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "📨 Project[{}] sending SessionPromptStart notification, session_id={}, request_id={:?}",
            project_id, session_id, request_id
        );

        // 观测：开启新一轮计时（覆盖同 key 的异常残留；不同 request 互不干扰）
        let key: RoundKey = (
            project_id.to_owned(),
            session_id.to_owned(),
            round_key_part(request_id.as_deref()),
        );
        self.round_metrics.insert(key, RoundMetrics::new());

        // 1. 更新状态为 Active（在推送 SSE 之前）
        self.update_agent_status(
            project_id,
            session_id,
            request_id.as_deref(),
            AgentStatus::Active,
        );

        // 2. 推送 SSE 消息
        self.inner
            .notify_prompt_start(project_id, session_id, request_id)
            .await
    }

    /// 推送会话结束通知
    ///
    /// 顺序：
    /// 1. 推送 SessionPromptEnd 到 SSE
    /// 2. 恢复 Agent 状态为 Idle
    async fn notify_prompt_end(
        &self,
        project_id: &str,
        session_id: &str,
        stop_reason: agent_client_protocol::schema::v1::StopReason,
        error_message: Option<String>,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "✅ Project[{}] sending SessionPromptEnd notification, session_id={}, stop_reason={:?}, request_id={:?}",
            project_id, session_id, stop_reason, request_id
        );

        // 1. 推送 SSE 消息（在恢复状态之前）
        let expected_request_id = request_id.clone();
        let result = self
            .inner
            .notify_prompt_end(
                project_id,
                session_id,
                stop_reason,
                error_message,
                request_id,
            )
            .await;

        // 2. 恢复状态为 Idle
        self.update_agent_status(
            project_id,
            session_id,
            expected_request_id.as_deref(),
            AgentStatus::Idle,
        );

        // 3. 观测：输出轮次耗时汇总（业务完成后执行，失败只丢统计行）
        // 按 (project, session, request) 精确移除：await 期间同一 session 可能
        // 已开启下一轮，旧轮的结束事件不能取走新轮的指标。
        let key: RoundKey = (
            project_id.to_owned(),
            session_id.to_owned(),
            round_key_part(expected_request_id.as_deref()),
        );
        if let Some((_, m)) = self.round_metrics.remove(&key) {
            let duration = m.start.elapsed();
            let first_chunk = m
                .first_chunk_at
                .map(|t| (t - m.start).as_secs_f64())
                .map(|s| format!("{:.1}s", s))
                .unwrap_or_else(|| "-".to_owned());
            let first_tool = m
                .first_tool_at
                .map(|t| (t - m.start).as_secs_f64())
                .map(|s| format!("{:.1}s", s))
                .unwrap_or_else(|| "-".to_owned());
            let line = format!(
                "[ROUND] project={} session={} duration={:.1}s first_chunk={} first_tool={} chunks={} tools={} stop={:?}",
                project_id,
                session_id,
                duration.as_secs_f64(),
                first_chunk,
                first_tool,
                m.chunks,
                m.tool_events,
                stop_reason
            );
            if duration >= self.slow_round_threshold {
                warn!(
                    "{} ⚠ SLOW ROUND (threshold={}s)",
                    line,
                    self.slow_round_threshold.as_secs()
                );
            } else {
                info!("{}", line);
            }
        }

        result
    }

    /// 推送会话错误通知
    ///
    /// 顺序：
    /// 1. 推送 SessionPromptError 到 SSE
    /// 2. 恢复 Agent 状态为 Idle
    async fn notify_prompt_error(
        &self,
        project_id: &str,
        session_id: &str,
        error: agent_client_protocol::schema::v1::Error,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        error!(
            "❌ Project[{}] sending SessionPromptError notification, session_id={}, error_code={}, error_message={}, request_id={:?}",
            project_id, session_id, error.code, error.message, request_id
        );

        // 1. 推送错误消息（在恢复状态之前）
        let expected_request_id = request_id.clone();
        let result = self
            .inner
            .notify_prompt_error(project_id, session_id, error, request_id)
            .await;

        // 2. 恢复状态为 Idle
        self.update_agent_status(
            project_id,
            session_id,
            expected_request_id.as_deref(),
            AgentStatus::Idle,
        );

        // 3. 观测：清理本轮计时（错误事件自身已由 error! 记录，不输出汇总）
        let key: RoundKey = (
            project_id.to_owned(),
            session_id.to_owned(),
            round_key_part(expected_request_id.as_deref()),
        );
        self.round_metrics.remove(&key);

        result
    }

    /// 推送 Agent 会话更新通知
    ///
    /// 不更新 Agent 状态，仅推送 SSE 消息。
    async fn notify_session_update(
        &self,
        project_id: &str,
        session_id: &str,
        session_update: agent_client_protocol::schema::v1::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "🔄 Project[{}] sending SessionUpdate notification, session_id={}",
            project_id, session_id
        );

        // 观测：更新轮次指标 + 工具事件骨架行（只在委托前借用，不改变转发行为）
        use agent_client_protocol::schema::v1::SessionUpdate as Su;
        // chunk/tool 事件归属哪一轮，由调用方传入的 request_id 决定：
        // 与 PromptStart 用同一个 key，换代期间的迟到事件才可能错轮。
        let key: RoundKey = (
            project_id.to_owned(),
            session_id.to_owned(),
            round_key_part(request_id.as_deref()),
        );
        match &session_update {
            Su::AgentMessageChunk(_) | Su::AgentThoughtChunk(_) => {
                // 高频 chunk：只聚合计数，不打日志（避免日志洪流）
                if let Some(mut m) = self.round_metrics.get_mut(&key) {
                    m.chunks += 1;
                    if m.first_chunk_at.is_none() {
                        m.first_chunk_at = Some(Instant::now());
                    }
                }
            }
            Su::UserMessageChunk(_) => {
                // 用户输入回显：不是 LLM 输出，不计 chunks/first_chunk（避免污染首 token 指标）
            }
            Su::ToolCall(tc) => {
                if let Some(mut m) = self.round_metrics.get_mut(&key) {
                    m.tool_events += 1;
                    if m.first_tool_at.is_none() {
                        m.first_tool_at = Some(Instant::now());
                    }
                }
                debug!(
                    "[TOOL] project={} session={} call={} tool={:?} kind={:?} status={:?} title={}",
                    project_id,
                    session_id,
                    tc.tool_call_id.0.as_ref(),
                    tc.name,
                    tc.kind,
                    tc.status,
                    brief_log(&tc.title, TOOL_TITLE_MAX_CHARS)
                );
            }
            Su::ToolCallUpdate(u) => {
                if let Some(mut m) = self.round_metrics.get_mut(&key) {
                    m.tool_events += 1;
                }
                debug!(
                    "[TOOL] project={} session={} call={} status={:?}",
                    project_id,
                    session_id,
                    u.tool_call_id.0.as_ref(),
                    u.fields.status
                );
            }
            _ => {}
        }

        // 委托给内部 notifier，不更新状态
        self.inner
            .notify_session_update(project_id, session_id, session_update, request_id)
            .await
    }

    /// 推送通用会话通知
    ///
    /// 不更新 Agent 状态，仅推送 SSE 消息。
    async fn notify(
        &self,
        project_id: &str,
        session_id: &str,
        notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "📢 Project[{}] sending generic session notification, session_id={}",
            project_id, session_id
        );

        // 委托给内部 notifier
        self.inner.notify(project_id, session_id, notify).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionUpdate as Su;
    use agent_client_protocol::schema::v1::{
        ContentChunk, PromptRequest, SessionId as AcpSessionId, StopReason, ToolCall,
        ToolCallUpdate, ToolCallUpdateFields,
    };
    use dashmap::mapref::entry::Entry;
    use shared_types::{
        AgentLifecycle, CancelNotificationRequestWrapper, ModelProviderConfig, SessionEntry,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// 测试用 key 构造（与生产的 (project, session, request) 三元组一致）。
    fn key_of(project: &str, session: &str, request: &str) -> RoundKey {
        (project.to_owned(), session.to_owned(), request.to_owned())
    }

    // ---- registry stub ----

    struct StubEntry {
        project_id: String,
        session_id: AcpSessionId,
        prompt_tx: mpsc::Sender<PromptRequest>,
        cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
        status: AgentStatus,
    }

    impl Clone for StubEntry {
        fn clone(&self) -> Self {
            Self {
                project_id: self.project_id.clone(),
                session_id: AcpSessionId::new(self.session_id.0.as_ref()),
                prompt_tx: self.prompt_tx.clone(),
                cancel_tx: self.cancel_tx.clone(),
                status: self.status,
            }
        }
    }

    impl SessionEntry for StubEntry {
        fn project_id(&self) -> &str {
            &self.project_id
        }
        fn session_id(&self) -> &AcpSessionId {
            &self.session_id
        }
        fn prompt_tx(&self) -> &mpsc::Sender<PromptRequest> {
            &self.prompt_tx
        }
        fn cancel_tx(&self) -> &mpsc::Sender<CancelNotificationRequestWrapper> {
            &self.cancel_tx
        }
        fn model_provider(&self) -> Option<&ModelProviderConfig> {
            None
        }
        fn lifecycle_handle(&self) -> Option<&Arc<dyn AgentLifecycle>> {
            None
        }
        fn status(&self) -> &AgentStatus {
            &self.status
        }
        fn is_channel_closed(&self) -> bool {
            false
        }
        fn is_model_config_changed(&self, _new: &Option<ModelProviderConfig>) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct StubRegistry {
        entries: DashMap<String, StubEntry>,
        routes: DashMap<String, String>,
        status_updates: AtomicUsize,
    }

    impl SessionRegistry for StubRegistry {
        type Entry = StubEntry;

        fn get(&self, project_id: &str) -> Option<StubEntry> {
            self.entries.get(project_id).map(|e| e.clone())
        }
        fn insert(&self, project_id: &str, session_id: &str, entry: StubEntry) {
            self.routes
                .insert(session_id.to_owned(), project_id.to_owned());
            self.entries.insert(project_id.to_owned(), entry);
        }
        fn remove(&self, project_id: &str) -> Option<StubEntry> {
            self.entries.remove(project_id).map(|(_, e)| e)
        }
        fn contains(&self, project_id: &str) -> bool {
            self.entries.contains_key(project_id)
        }
        fn get_project_by_session(&self, session_id: &str) -> Option<String> {
            self.routes.get(session_id).map(|p| p.clone())
        }
        fn get_entry_by_session(&self, session_id: &str) -> Option<StubEntry> {
            self.get(&self.get_project_by_session(session_id)?)
        }
        fn list_project_ids(&self) -> Vec<String> {
            self.entries.iter().map(|e| e.key().clone()).collect()
        }
        fn count(&self) -> usize {
            self.entries.len()
        }
        fn entry(&self, project_id: String) -> Entry<'_, String, StubEntry> {
            self.entries.entry(project_id)
        }
        fn update_agent_status_for_session(
            &self,
            _project_id: &str,
            _session_id: &str,
            _request_id: Option<&str>,
            _status: AgentStatus,
        ) {
            self.status_updates.fetch_add(1, Ordering::SeqCst);
        }
    }

    // ---- StateAwareNotifier 轮次指标 ----

    fn notifier(threshold_secs: u64) -> StateAwareNotifier<StubRegistry> {
        StateAwareNotifier::with_slow_round_threshold_secs(
            Arc::new(StubRegistry::default()),
            threshold_secs,
        )
    }

    fn chunk_update(text: &str) -> Su {
        Su::AgentMessageChunk(ContentChunk::new(text.to_owned().into()))
    }

    #[tokio::test]
    async fn round_flow_counts_and_clears() {
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_a", Some("req-1".into()))
            .await
            .unwrap();

        n.notify_session_update("633", "ses_a", chunk_update("你"), Some("req-1".into()))
            .await
            .unwrap();
        n.notify_session_update("633", "ses_a", chunk_update("好"), Some("req-1".into()))
            .await
            .unwrap();
        n.notify_session_update(
            "633",
            "ses_a",
            Su::ToolCall(ToolCall::new("call_1", "读取文件")),
            Some("req-1".into()),
        )
        .await
        .unwrap();
        n.notify_session_update(
            "633",
            "ses_a",
            Su::ToolCallUpdate(ToolCallUpdate::new(
                "call_1",
                ToolCallUpdateFields::default(),
            )),
            Some("req-1".into()),
        )
        .await
        .unwrap();
        n.notify_session_update("633", "ses_a", chunk_update("结果"), Some("req-1".into()))
            .await
            .unwrap();

        {
            let m = n
                .round_metrics
                .get(&key_of("633", "ses_a", "req-1"))
                .expect("metrics 存在");
            assert_eq!(m.chunks, 3);
            assert_eq!(m.tool_events, 2);
            assert!(m.first_chunk_at.is_some());
            assert!(m.first_tool_at.is_some());
        }

        n.notify_prompt_end(
            "633",
            "ses_a",
            StopReason::EndTurn,
            None,
            Some("req-1".into()),
        )
        .await
        .unwrap();
        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_a", "req-1")),
            "PromptEnd 后 metrics 应被清理"
        );
    }

    #[tokio::test]
    async fn prompt_error_clears_metrics() {
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_b", Some("req-1".into()))
            .await
            .unwrap();
        n.notify_session_update("633", "ses_b", chunk_update("x"), Some("req-1".into()))
            .await
            .unwrap();

        let err = agent_client_protocol::schema::v1::Error::new(-32000, "boom");
        n.notify_prompt_error("633", "ses_b", err, Some("req-1".into()))
            .await
            .unwrap();
        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_b", "req-1")),
            "PromptError 后 metrics 应被清理"
        );
    }

    #[tokio::test]
    async fn repeated_start_same_request_overwrites_previous_round() {
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_c", Some("req-1".into()))
            .await
            .unwrap();
        n.notify_session_update("633", "ses_c", chunk_update("x"), Some("req-1".into()))
            .await
            .unwrap();
        // 异常时序：未 end 直接再来同一轮 start（同 request）
        n.notify_prompt_start("633", "ses_c", Some("req-1".into()))
            .await
            .unwrap();

        let m = n
            .round_metrics
            .get(&key_of("633", "ses_c", "req-1"))
            .expect("metrics 存在");
        assert_eq!(m.chunks, 0, "重复 start 应重置计数");
        assert!(m.first_chunk_at.is_none());
    }

    #[tokio::test]
    async fn late_end_does_not_take_new_round_metrics() {
        // 跨请求污染反例：旧轮的 end 在 await 内部 notifier 之后才恢复，
        // 这期间同一 session 已开启新轮。旧轮 end 只能移除自己的指标。
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_g", Some("req-old".into()))
            .await
            .unwrap();
        n.notify_session_update(
            "633",
            "ses_g",
            chunk_update("旧轮输出"),
            Some("req-old".into()),
        )
        .await
        .unwrap();

        // 换代：新轮开启并产生自己的指标
        n.notify_prompt_start("633", "ses_g", Some("req-new".into()))
            .await
            .unwrap();
        n.notify_session_update(
            "633",
            "ses_g",
            chunk_update("新轮输出"),
            Some("req-new".into()),
        )
        .await
        .unwrap();

        // 旧轮的结束事件迟到
        n.notify_prompt_end(
            "633",
            "ses_g",
            StopReason::EndTurn,
            None,
            Some("req-old".into()),
        )
        .await
        .unwrap();

        {
            let m = n
                .round_metrics
                .get(&key_of("633", "ses_g", "req-new"))
                .expect("新轮指标必须保留");
            assert_eq!(m.chunks, 1, "旧轮 end 取走了新轮的 chunk 计数");
            assert!(
                m.first_chunk_at.is_some(),
                "旧轮 end 清掉了新轮的首 token 时刻"
            );
        }
        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_g", "req-old")),
            "旧轮指标应被自己的 end 清掉"
        );
    }

    #[tokio::test]
    async fn late_error_does_not_take_new_round_metrics() {
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_h", Some("req-old".into()))
            .await
            .unwrap();
        n.notify_prompt_start("633", "ses_h", Some("req-new".into()))
            .await
            .unwrap();
        n.notify_session_update("633", "ses_h", chunk_update("新轮"), Some("req-new".into()))
            .await
            .unwrap();

        let err = agent_client_protocol::schema::v1::Error::new(-32000, "boom");
        n.notify_prompt_error("633", "ses_h", err, Some("req-old".into()))
            .await
            .unwrap();

        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_h", "req-old")),
            "旧轮指标应被自己的 error 清掉"
        );
        let m = n
            .round_metrics
            .get(&key_of("633", "ses_h", "req-new"))
            .expect("新轮指标必须保留");
        assert_eq!(m.chunks, 1);
    }

    #[tokio::test]
    async fn update_without_start_is_ignored_gracefully() {
        let n = notifier(120);
        // 无 PromptStart 直接 update / end：不 panic、不留残留
        n.notify_session_update("633", "ses_d", chunk_update("x"), None)
            .await
            .unwrap();
        n.notify_prompt_end("633", "ses_d", StopReason::EndTurn, None, None)
            .await
            .unwrap();
        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_d", "req-1"))
        );
    }

    #[tokio::test]
    async fn zero_threshold_marks_slow_round() {
        // threshold=0 → 任何轮次都走 WARN 分支（只验证执行路径不 panic）
        let n = notifier(0);
        n.notify_prompt_start("633", "ses_e", Some("req-1".into()))
            .await
            .unwrap();
        n.notify_session_update("633", "ses_e", chunk_update("x"), Some("req-1".into()))
            .await
            .unwrap();
        n.notify_prompt_end(
            "633",
            "ses_e",
            StopReason::EndTurn,
            None,
            Some("req-1".into()),
        )
        .await
        .unwrap();
        assert!(
            !n.round_metrics
                .contains_key(&key_of("633", "ses_e", "req-1"))
        );
    }

    #[tokio::test]
    async fn user_message_chunk_not_counted_as_llm_output() {
        let n = notifier(120);
        n.notify_prompt_start("633", "ses_f", Some("req-1".into()))
            .await
            .unwrap();
        // 用户输入回显到达（先于 LLM 输出）：不得计入 chunks / first_chunk
        n.notify_session_update(
            "633",
            "ses_f",
            Su::UserMessageChunk(ContentChunk::new("用户输入".into())),
            Some("req-1".into()),
        )
        .await
        .unwrap();
        {
            let m = n
                .round_metrics
                .get(&key_of("633", "ses_f", "req-1"))
                .expect("metrics 存在");
            assert_eq!(m.chunks, 0, "用户输入回显不应计入 LLM 输出");
            assert!(m.first_chunk_at.is_none(), "first_chunk 不得被用户输入污染");
        }
        // 随后真正的 LLM chunk 正常计入
        n.notify_session_update("633", "ses_f", chunk_update("输出"), Some("req-1".into()))
            .await
            .unwrap();
        let m = n
            .round_metrics
            .get(&key_of("633", "ses_f", "req-1"))
            .expect("metrics 存在");
        assert_eq!(m.chunks, 1);
        assert!(m.first_chunk_at.is_some());
    }
}

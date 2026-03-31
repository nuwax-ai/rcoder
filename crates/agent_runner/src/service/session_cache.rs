//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use dashmap::DashMap;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::AGENT_REGISTRY;

/// 日志截取的最大长度默认值
const MAX_LOG_TRUNCATE_LEN: usize = 50;

/// 截取消息内容用于日志打印（防止日志膨胀）
///
/// 社区常见做法：
/// - tracing: 通过 Subscriber 配置限制字段大小
/// - serde: 自定义 Serialize 实现截断
/// - 简单场景: chars().take() + 长度检查（本实现）
fn truncate_message_for_log(data: &serde_json::Value, max_len: usize) -> String {
    // 边界检查：max_len 为 0 时返回空，避免无效计算
    if max_len == 0 {
        return String::new();
    }

    // 优先提取原始字符串内容，避免 JSON 序列化后的引号包裹
    let s = match data.as_str() {
        Some(inner) => inner.to_string(),
        None => data.to_string(),
    };

    // chars().count() 是一次性遍历，可以与 take() 合并优化但牺牲可读性
    // 当前实现清晰且性能可接受（短字符串场景）
    let char_count = s.chars().count();
    if char_count <= max_len {
        return s;
    }

    // 使用 chars() 安全截取 UTF-8 字符边界
    let truncated: String = s.chars().take(max_len).collect();
    format!("{}... (truncated)", truncated)
}

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, Arc<SessionData>>> = LazyLock::new(DashMap::new);

/// Session数据包装 - 极简版本，专注消息传输
pub struct SessionData {
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    // 🎯 极简优化：直接存储当前连接，无需命令传递
    current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
    current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
}

impl SessionData {
    pub fn new(max_size: usize) -> Arc<Self> {
        let start_time = std::time::Instant::now();
        debug!("⏱️ [SessionData::new] Starting creation, max_size={}", max_size);

        let channel_start = std::time::Instant::now();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        debug!(
            "⏱️ [SessionData::new] Channel creation took: {:?}",
            channel_start.elapsed()
        );

        let arc_start = std::time::Instant::now();
        let session = Arc::new(SessionData {
            command_tx,
            current_sender: Arc::new(tokio::sync::Mutex::new(None)),
            current_cancel: Arc::new(tokio::sync::Mutex::new(None)),
        });
        debug!(
            "⏱️ [SessionData::new] Arc creation took: {:?}",
            arc_start.elapsed()
        );

        let spawn_start = std::time::Instant::now();
        SessionWorker::spawn(
            max_size,
            command_rx,
            session.current_sender.clone(),
            session.current_cancel.clone(),
        );
        debug!(
            "⏱️ [SessionData::new] SessionWorker::spawn took: {:?}",
            spawn_start.elapsed()
        );

        debug!(
            "⏱️ [SessionData::new] Total creation took: {:?}",
            start_time.elapsed()
        );
        session
    }

    pub async fn message_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::MessageCount { ack: tx })
            .is_err()
        {
            warn!("Failed to send message_count command; worker has exited");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub async fn create_new_connection(
        &self,
        buffer_size: usize,
    ) -> Result<(mpsc::Receiver<UnifiedSessionMessage>, CancellationToken)> {
        let start_time = std::time::Instant::now();
        debug!(
            "⏱️ [create_new_connection] Starting connection creation, buffer_size={}",
            buffer_size
        );

        let token_start = std::time::Instant::now();
        let cancellation_token = CancellationToken::new();
        debug!(
            "⏱️ [create_new_connection] CancellationToken creation took: {:?}",
            token_start.elapsed()
        );

        let channel_start = std::time::Instant::now();
        let (tx, rx) = mpsc::channel(buffer_size);
        debug!(
            "⏱️ [create_new_connection] mpsc channel creation took: {:?}",
            channel_start.elapsed()
        );

        let setup_start = std::time::Instant::now();
        // 🛡️ 关键修复：使用 lock() 而非 try_lock()，确保连接一定被设置
        // try_lock() 可能失败导致 current_sender 未设置，造成消息丢失
        {
            // 取消之前的连接
            let mut current_cancel_guard = self.current_cancel.lock().await;
            if let Some(token) = current_cancel_guard.take() {
                token.cancel();
            }
            // 设置新的取消令牌
            *current_cancel_guard = Some(cancellation_token.clone());

            // 设置新的发送器
            let mut current_sender_guard = self.current_sender.lock().await;
            *current_sender_guard = Some(tx);
        }
        debug!(
            "⏱️ [create_new_connection] Connection state setup took: {:?}",
            setup_start.elapsed()
        );

        debug!(
            "⏱️ [create_new_connection] Total connection creation took: {:?}",
            start_time.elapsed()
        );
        Ok((rx, cancellation_token))
    }

    pub fn push_message(&self, message: UnifiedSessionMessage) {
        if self
            .command_tx
            .send(SessionCommand::Push { message })
            .is_err()
        {
            warn!("Failed to push message; worker has exited");
        }
    }

    /// 主动关闭当前 SSE 连接
    ///
    /// 当用户取消任务时，需要主动关闭 SSE 连接，而不是让客户端一直等待
    ///
    /// 关闭机制：
    /// 1. 触发 CancellationToken，让 SSE 流立即退出循环
    /// 2. 显式关闭 channel 发送端，让 rx.recv() 立即返回 None
    /// 3. 清空连接状态，防止新的消息被发送
    pub async fn close_current_connection(&self) {
        // 🎯 主动触发取消令牌，关闭 SSE 连接
        let mut current_cancel_guard = self.current_cancel.lock().await;
        if let Some(token) = current_cancel_guard.take() {
            info!("🔌 [SessionData] Triggering CancellationToken to close SSE connection");
            token.cancel();
        }
        drop(current_cancel_guard);

        // 🎯 显式关闭 channel 发送端，让接收端立即感知到连接关闭
        let mut current_sender_guard = self.current_sender.lock().await;
        if current_sender_guard.take().is_some() {
            info!("🔌 [SessionData] Explicitly closed channel sender; receiver disconnects immediately");
            // 当 Sender 被 drop 时，Receiver 的 recv() 会返回 None
            // 这里通过 take() 将 sender 从 Option 中移除，触发 drop
        }
    }
}

struct SessionWorker {
    max_size: usize,
    command_rx: mpsc::UnboundedReceiver<SessionCommand>,
    // 🎯 极简优化：直接共享连接状态，无需命令传递
    current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
    current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
}

impl SessionWorker {
    fn spawn(
        max_size: usize,
        command_rx: mpsc::UnboundedReceiver<SessionCommand>,
        current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
        current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
    ) {
        let start_time = std::time::Instant::now();
        debug!(
            "⏱️ [SessionWorker::spawn] Starting SessionWorker creation, max_size={}",
            max_size
        );

        let worker = SessionWorker {
            max_size,
            command_rx,
            current_sender,
            current_cancel,
        };

        let spawn_start = std::time::Instant::now();
        tokio::spawn(worker.run());
        debug!(
            "⏱️ [SessionWorker::spawn] tokio::spawn took: {:?}",
            spawn_start.elapsed()
        );
        debug!(
            "⏱️ [SessionWorker::spawn] Total spawn took: {:?}",
            start_time.elapsed()
        );
    }

    async fn run(mut self) {
        let (mut producer, mut consumer) = HeapRb::new(self.max_size).split();
        let mut buffered_len = 0usize;

        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                SessionCommand::Push { message } => {
                    let should_buffer = !matches!(
                        message.message_type,
                        crate::model::SessionMessageType::Heartbeat
                    );

                    if should_buffer {
                        if producer.is_full() {
                            let _ = consumer.try_pop();
                            buffered_len = buffered_len.saturating_sub(1);
                        }
                        if producer.try_push(message.clone()).is_ok() {
                            buffered_len += 1;
                        } else {
                            warn!("Ring buffer push failed; real-time delivery only");
                        }
                    }

                    // 🛡️ 关键修复：使用 lock().await 确保消息一定被发送
                    // try_lock() 可能失败导致消息丢失，造成 SSE 卡死
                    let mut current_sender_guard = self.current_sender.lock().await;
                    if let Some(sender) = current_sender_guard.as_mut() {
                        if sender.try_send(message.clone()).is_err() {
                            // 如果发送失败，可能是缓冲区满了或连接已关闭
                            // 注意：truncate 在锁内执行，但 50 字符开销可忽略
                            warn!(
                            "⚠️ SSE sender send failed, disabling real-time delivery: message_type={:?}, sub_type={}, data={}",
                            message.message_type,
                            message.sub_type,
                            truncate_message_for_log(&message.data, MAX_LOG_TRUNCATE_LEN)
                            );
                            *current_sender_guard = None;
                        }
                    } else {
                        // 连接不存在，跳过实时推送（记录为 info 级别，便于排查问题）
                        info!(
                            "📭 SSE sender missing, skipping real-time delivery (message buffered in ring buffer): message_type={:?}, sub_type={}, data={}",
                            message.message_type,
                            message.sub_type,
                            truncate_message_for_log(&message.data, MAX_LOG_TRUNCATE_LEN)
                        );
                    }
                }
                SessionCommand::Clear { ack } => {
                    let mut cleared = 0usize;
                    while consumer.try_pop().is_some() {
                        cleared += 1;
                    }
                    buffered_len = 0;
                    let _ = ack.send(cleared);
                }
                SessionCommand::MessageCount { ack } => {
                    let _ = ack.send(buffered_len);
                }
            }
        }

        debug!("🔚 SessionWorker stopped");
    }
}

#[derive(Debug)]
enum SessionCommand {
    Push { message: UnifiedSessionMessage },
    Clear { ack: oneshot::Sender<usize> },
    MessageCount { ack: oneshot::Sender<usize> },
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
///
/// 如果 SESSION_CACHE 中不存在该 session_id 的条目，会自动创建。
/// 这解决了 Agent 开始推送消息时 SESSION_CACHE 条目尚未由 HTTP 处理器创建的竞态问题。
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    use dashmap::mapref::entry::Entry;

    // 🛡️ 关键修复：使用 entry API 原子性地获取或创建 SessionData
    // 之前直接用 get()，如果 session 不存在就丢弃消息。
    // 竞态场景：Agent 开始推送消息 → push_session_update 查找 SESSION_CACHE → 不存在（因为
    // handle_chat_core 尚未返回，computer_chat.rs 还未创建条目）→ 消息被丢弃
    let session_data = {
        match SESSION_CACHE.entry(session_id.to_string()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let data = SessionData::new(1000);
                info!(
                    "📦 [push_session_update] SESSION_CACHE auto-created: session_id={}",
                    session_id
                );
                entry.insert(data.clone());
                data
            }
        }
    };

    let unified_message = notify.to_unified_message();

    debug!(
        "📥 Pushing message to cache: session_id={}, message_type={:?}, sub_type={}",
        session_id, unified_message.message_type, unified_message.sub_type
    );

    session_data.push_message(unified_message);

    Ok(())
}

/// 便捷函数：添加SessionNotify消息并管理Project-Session映射
///
/// 这个函数会自动确保project_id只对应一个活跃的session_id
///
/// 这个函数会自动确保project_id只对应一个活跃的session_id
/// 当检测到session_id变化时，会自动清理旧session的数据
pub async fn push_session_update_with_project(
    project_id: &str,
    session_id: &str,
    notify: SessionNotify,
) -> Result<()> {
    // 确保project_id对应正确的session_id，如果变化则清理旧数据
    let cleared_count = ensure_project_session(project_id, session_id).await;

    if cleared_count > 0 {
        info!(
            "📝 [push_session_update_with_project] Session changed, cleaned {} old messages: project_id={}, new_session_id={}",
            cleared_count, project_id, session_id
        );
    }

    // 推送消息到新的session
    push_session_update(session_id, notify).await
}

/// 确保project_id对应正确的session_id
///
/// 使用统一的 AGENT_REGISTRY 管理 project-session 映射
/// 如果project_id对应的session_id发生变化，会自动清理旧session的数据
/// 如果session_id相同，则不做任何操作
///
/// 参数:
/// - project_id: 项目ID
/// - session_id: 当前会话ID
///
/// 返回值: 如果清理了旧数据则返回清理的消息数量，否则返回0
pub async fn ensure_project_session(project_id: &str, session_id: &str) -> usize {
    // 使用统一 Registry 检查当前映射
    let mapped_session_id = AGENT_REGISTRY.get_session_by_project(project_id);

    match mapped_session_id {
        Some(mapped_sid) if mapped_sid == session_id => {
            // session_id 相同，不需要做任何操作
            debug!(
                "📋 Project session mapping unchanged: project_id={}, session_id={}",
                project_id, session_id
            );
            0
        }
        Some(old_session_id) => {
            // session_id 发生变化，需要清理旧 session 的数据
            info!(
                "🔄 Detected project session change: project_id={}, old_session_id={}, new_session_id={}",
                project_id, old_session_id, session_id
            );

            // 🛡️ 关键修复：先主动关闭旧 session 的 SSE 连接，再移除缓存
            // 之前直接 remove 导致旧 SSE 连接的心跳流继续发送但不再收到业务消息，
            // 前端如果没有及时关闭旧连接，会看到孤立的心跳流
            let cleared_count =
                if let Some((_, old_session_data)) = SESSION_CACHE.remove(&old_session_id) {
                    old_session_data.close_current_connection().await;
                    info!(
                        "🔌 [ensure_project_session] Closed old session SSE connection: old_session_id={}",
                        old_session_id
                    );
                    1 // 移除了1个session
                } else {
                    0 // session不存在
                };

            // 更新 AGENT_REGISTRY 中的映射关系
            let _ = AGENT_REGISTRY.update_session(project_id, session_id);

            if cleared_count > 0 {
                info!(
                    "🧹 Cleared old session data and updated mapping: project_id={}, old_session_id={}, new_session_id={}, cleared_count={}",
                    project_id, old_session_id, session_id, cleared_count
                );
            } else {
                info!(
                    "📝 Updated project session mapping: project_id={}, old_session_id={}, new_session_id={}",
                    project_id, old_session_id, session_id
                );
            }

            cleared_count
        }
        None => {
            // 第一次建立映射关系（无旧映射）
            // 注意：此时 AGENT_REGISTRY 中可能还没有这个 project 的记录
            // 这种情况下不需要调用 update_session，因为 agent 注册时会调用 register
            info!(
                "🆕 Project session first seen: project_id={}, session_id={}",
                project_id, session_id
            );
            0
        }
    }
}

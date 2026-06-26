//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

#![allow(dead_code)]

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

/// Session命令通道的缓冲区大小
///
/// 与 ring buffer 大小一致，提供足够的缓冲同时防止 OOM
const COMMAND_CHANNEL_BUFFER_SIZE: usize = 1000;

/// Session数据包装 - 极简版本，专注消息传输
pub struct SessionData {
    command_tx: mpsc::Sender<SessionCommand>,
    // 🎯 极简优化：直接存储当前连接，无需命令传递
    current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
    current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
    // 🔒 Critical fix: 存储 worker JoinHandle，用于检测 panic
    worker_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SessionData {
    pub async fn new(max_size: usize) -> Arc<Self> {
        let start_time = std::time::Instant::now();
        debug!(
            "[SessionData::new] Starting creation, max_size={}",
            max_size
        );

        let channel_start = std::time::Instant::now();
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_BUFFER_SIZE);
        debug!(
            "[SessionData::new] Channel creation took: {:?}",
            channel_start.elapsed()
        );

        let arc_start = std::time::Instant::now();
        let session = Arc::new(SessionData {
            command_tx,
            current_sender: Arc::new(tokio::sync::Mutex::new(None)),
            current_cancel: Arc::new(tokio::sync::Mutex::new(None)),
            worker_handle: Arc::new(tokio::sync::Mutex::new(None)),
        });
        debug!(
            "[SessionData::new] Arc creation took: {:?}",
            arc_start.elapsed()
        );

        let spawn_start = std::time::Instant::now();
        let handle = SessionWorker::spawn(
            max_size,
            command_rx,
            session.current_sender.clone(),
            session.current_cancel.clone(),
        );

        // 🔒 Critical fix: 使用 async lock 替代 blocking_lock，避免阻塞 executor
        {
            let mut worker_guard = session.worker_handle.lock().await;
            *worker_guard = Some(handle);
        }

        debug!(
            "[SessionData::new] SessionWorker::spawn took: {:?}",
            spawn_start.elapsed()
        );

        debug!(
            "[SessionData::new] Total creation took: {:?}",
            start_time.elapsed()
        );
        session
    }

    pub async fn message_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::MessageCount { ack: tx })
            .await
            .is_err()
        {
            warn!("Failed to send message_count command; worker has exited");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// 获取 ring buffer 中所有消息的快照（非破坏性读取）
    ///
    /// 用于 SSE 连接建立时回放历史消息给新客户端
    pub async fn replay_buffer(&self) -> Vec<UnifiedSessionMessage> {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::Replay { ack: tx })
            .await
            .is_err()
        {
            warn!("Failed to send replay command; worker has exited");
            return vec![];
        }
        rx.await.unwrap_or_default()
    }

    /// 清空 ring buffer 中所有消息
    ///
    /// 新对话开始时调用，防止回放过期的历史消息
    pub async fn clear_message_buffer(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::Clear { ack: tx })
            .await
            .is_err()
        {
            warn!("Failed to send clear command; worker has exited");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// 检查 worker 是否仍然存活
    ///
    /// 如果 worker panic 或正常退出，返回 false
    pub async fn is_worker_alive(&self) -> bool {
        let mut guard = self.worker_handle.lock().await;
        match guard.as_mut() {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }

    /// 检查 worker 是否 panic
    ///
    /// 如果 worker 已经 panic，返回 true 并记录错误信息
    pub async fn has_worker_panicked(&self) -> bool {
        let mut guard = self.worker_handle.lock().await;
        if let Some(handle) = guard.as_ref() {
            if handle.is_finished() {
                // take() 消耗 handle 来 await 获取结果
                let handle = guard.take().unwrap();
                match handle.await {
                    Err(e) if e.is_panic() => {
                        warn!("[SessionData] SessionWorker panicked: {:?}", e);
                        true
                    }
                    Err(e) if e.is_cancelled() => {
                        debug!("[SessionData] SessionWorker was cancelled");
                        false
                    }
                    Ok(_) => {
                        debug!("[SessionData] SessionWorker exited normally");
                        false
                    }
                    _ => false,
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    pub async fn create_new_connection(
        &self,
        buffer_size: usize,
    ) -> Result<(
        Vec<UnifiedSessionMessage>,
        mpsc::Receiver<UnifiedSessionMessage>,
        CancellationToken,
    )> {
        let start_time = std::time::Instant::now();
        debug!(
            "[create_new_connection] Starting connection creation, buffer_size={}",
            buffer_size
        );

        let token_start = std::time::Instant::now();
        let cancellation_token = CancellationToken::new();
        debug!(
            "[create_new_connection] CancellationToken creation took: {:?}",
            token_start.elapsed()
        );

        let channel_start = std::time::Instant::now();
        let (tx, rx) = mpsc::channel(buffer_size);
        debug!(
            "[create_new_connection] mpsc channel creation took: {:?}",
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
            "[create_new_connection] Connection state setup took: {:?}",
            setup_start.elapsed()
        );

        // 📼 回放 ring buffer 中的历史消息（在设置 current_sender 之后）
        // 确保快照包含设置 current_sender 之前缓冲的所有消息
        // 时序保证：
        // 1. 设置 current_sender 后，新消息会通过 channel 发送
        // 2. replay_buffer() 获取的是设置 current_sender 之前缓冲的消息
        // 3. 这些消息不会通过 current_sender 发送，所以需要回放
        let replay_start = std::time::Instant::now();
        let replay_messages = self.replay_buffer().await;
        if !replay_messages.is_empty() {
            info!(
                "[create_new_connection] Replaying {} buffered messages",
                replay_messages.len()
            );
        }
        debug!(
            "[create_new_connection] Replay took: {:?}",
            replay_start.elapsed()
        );

        debug!(
            "[create_new_connection] Total connection creation took: {:?}",
            start_time.elapsed()
        );
        Ok((replay_messages, rx, cancellation_token))
    }

    /// 检查 worker 是否已完成 (non-blocking)
    ///
    /// 如果 worker 已完成（正常退出、取消或 panic），返回 true
    /// 如果无法获取锁或 worker 仍在运行，返回 false
    ///
    /// 注意：此方法无法区分正常退出和 panic，需要调用 async 版本的
    /// `has_worker_panicked()` 来获取准确的 panic 检测结果
    pub fn is_worker_finished_nonblocking(&self) -> bool {
        // 使用 try_lock 避免阻塞
        if let Ok(guard) = self.worker_handle.try_lock()
            && let Some(handle) = guard.as_ref()
        {
            return handle.is_finished();
        }
        false
    }

    pub fn push_message(&self, message: UnifiedSessionMessage) {
        // 使用 try_send 提供背压保护：通道满时丢弃消息而不是无限增长
        // 这是安全的，因为：
        // 1. ring buffer 已经缓存了最近的消息（用于重连时恢复）
        // 2. SSE 客户端断线重连后会从 ring buffer 获取历史消息
        if self
            .command_tx
            .try_send(SessionCommand::Push { message })
            .is_err()
        {
            // 检查是否因为 worker 已完成导致通道关闭
            if self.is_worker_finished_nonblocking() {
                warn!(
                    "[SessionData] Failed to push message: SessionWorker has exited (normal exit, cancelled, or panicked). Messages will be lost until session is recreated"
                );
            } else {
                warn!("Failed to push message: command channel full (backpressure)");
            }
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
            info!("[SessionData] Triggering CancellationToken to close SSE connection");
            token.cancel();
        }
        drop(current_cancel_guard);

        // 🎯 显式关闭 channel 发送端，让接收端立即感知到连接关闭
        let mut current_sender_guard = self.current_sender.lock().await;
        if current_sender_guard.take().is_some() {
            info!(
                "[SessionData] Explicitly closed channel sender; receiver disconnects immediately"
            );
            // 当 Sender 被 drop 时，Receiver 的 recv() 会返回 None
            // 这里通过 take() 将 sender 从 Option 中移除，触发 drop
        }
    }
}

struct SessionWorker {
    max_size: usize,
    command_rx: mpsc::Receiver<SessionCommand>,
    // 🎯 极简优化：直接共享连接状态，无需命令传递
    current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
    current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
}

impl SessionWorker {
    fn spawn(
        max_size: usize,
        command_rx: mpsc::Receiver<SessionCommand>,
        current_sender: Arc<tokio::sync::Mutex<Option<mpsc::Sender<UnifiedSessionMessage>>>>,
        current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
    ) -> tokio::task::JoinHandle<()> {
        let start_time = std::time::Instant::now();
        debug!(
            "[SessionWorker::spawn] Starting SessionWorker creation, max_size={}",
            max_size
        );

        let worker = SessionWorker {
            max_size,
            command_rx,
            current_sender,
            current_cancel,
        };

        let spawn_start = std::time::Instant::now();
        let handle = tokio::spawn(worker.run());
        debug!(
            "[SessionWorker::spawn] tokio::spawn took: {:?}",
            spawn_start.elapsed()
        );
        debug!(
            "[SessionWorker::spawn] Total spawn took: {:?}",
            start_time.elapsed()
        );

        handle
    }

    async fn run(mut self) {
        let (mut producer, mut consumer) = HeapRb::new(self.max_size).split();
        let mut buffered_len = 0usize;

        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                SessionCommand::Push { message } => {
                    debug!(
                        "[SessionWorker] Push message: message_type={:?}, sub_type={}, data={}",
                        message.message_type,
                        message.sub_type,
                        message.data
                    );

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
                        use tokio::sync::mpsc::error::TrySendError;
                        if let Err(send_err) = sender.try_send(message.clone()) {
                            match send_err {
                                TrySendError::Full(_) => {
                                    // buffer 满（客户端暂时慢）：不禁用 sender，等客户端消费后恢复
                                    // 消息已在 ring buffer 中备份，不会真正丢失
                                    warn!(
                                        "SSE sender buffer full, message buffered: message_type={:?}, sub_type={}",
                                        message.message_type, message.sub_type,
                                    );
                                }
                                TrySendError::Closed(_) => {
                                    // receiver 已断开：禁用 sender，避免后续每条消息都 try_send 失败
                                    // SubscribeProgress 会在 recv() 返回 None 时检测到并清理
                                    warn!(
                                        "SSE sender receiver dropped, disabling sender: message_type={:?}, sub_type={}",
                                        message.message_type, message.sub_type,
                                    );
                                    *current_sender_guard = None;
                                }
                            }
                        }
                    } else {
                        // 连接不存在，跳过实时推送（记录为 info 级别，便于排查问题）
                        info!(
                            "SSE sender missing, skipping real-time delivery (message buffered in ring buffer): message_type={:?}, sub_type={}, data={}",
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
                SessionCommand::Replay { ack } => {
                    let count = consumer.occupied_len();
                    let mut snapshot = Vec::with_capacity(count);
                    // Pop all items from ring buffer
                    while let Some(msg) = consumer.try_pop() {
                        snapshot.push(msg);
                    }
                    // Push them back (preserving order)
                    let mut pushed_back = 0;
                    for msg in &snapshot {
                        if producer.try_push(msg.clone()).is_ok() {
                            pushed_back += 1;
                        } else {
                            break;
                        }
                    }
                    buffered_len = pushed_back;
                    debug!(
                        "[SessionWorker] Replay: captured {} messages from ring buffer (occupied={}, pushed_back={})",
                        snapshot.len(),
                        count,
                        pushed_back
                    );
                    let _ = ack.send(snapshot);
                }
            }
        }

        debug!("[SessionWorker] stopped");
    }
}

#[derive(Debug)]
enum SessionCommand {
    Push {
        message: UnifiedSessionMessage,
    },
    Clear {
        ack: oneshot::Sender<usize>,
    },
    MessageCount {
        ack: oneshot::Sender<usize>,
    },
    Replay {
        ack: oneshot::Sender<Vec<UnifiedSessionMessage>>,
    },
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
///
/// 如果 SESSION_CACHE 中不存在该 session_id 的条目，会自动创建。
/// 这解决了 Agent 开始推送消息时 SESSION_CACHE 条目尚未由 HTTP 处理器创建的竞态问题。
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    use dashmap::mapref::entry::Entry;

    // 🛡️ 关键修复：不在 DashMap entry() 持锁范围内调用 .await
    // 之前 .await 在 entry() 作用域内执行，持有 shard 写锁跨 yield point，
    // 导致同 shard 的所有并发操作被阻塞（包括其他 session 的 push/get）。
    //
    // 修复策略：
    // 1. 快速路径：get() + 检查（只持 shard 读锁，不在锁内 await）
    // 2. 慢速路径：先在 entry() 外部 await 创建 SessionData，再原子插入

    // 快速路径：session 存在 → 检查 worker 状态 → 推送
    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(existing) = SESSION_CACHE.view(session_id, |_, d| d.clone()) {
        if existing.has_worker_panicked().await {
            // Worker panic：在 entry() 外部创建新 SessionData
            warn!(
                "[push_session_update] SessionWorker panicked for session_id={}, recreating...",
                session_id
            );
            let new_data = SessionData::new(1000).await;
            // entry API 原子替换（语义更明确）
            SESSION_CACHE
                .entry(session_id.to_string())
                .and_modify(|d| *d = new_data.clone())
                .or_insert_with(|| new_data.clone());
            new_data.push_message(notify.to_unified_message());
        } else {
            existing.push_message(notify.to_unified_message());
        }
        return Ok(());
    }

    // 慢速路径：session 不存在 → 在 entry() 外部 await 创建
    let data = SessionData::new(1000).await;
    info!(
        "[push_session_update] SESSION_CACHE auto-created: session_id={}",
        session_id
    );

    // 原子插入（如果其他任务先创建了，则使用现有的）
    let session_data = match SESSION_CACHE.entry(session_id.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            entry.insert(data.clone());
            data
        }
    };

    session_data.push_message(notify.to_unified_message());
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
            "[push_session_update_with_project] Session changed, cleaned {} old messages: project_id={}, new_session_id={}",
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
                "Project session mapping unchanged: project_id={}, session_id={}",
                project_id, session_id
            );
            0
        }
        Some(old_session_id) => {
            // session_id 发生变化，需要清理旧 session 的数据
            info!(
                "Detected project session change: project_id={}, old_session_id={}, new_session_id={}",
                project_id, old_session_id, session_id
            );

            // 🛡️ 关键修复：先主动关闭旧 session 的 SSE 连接，再移除缓存
            // 之前直接 remove 导致旧 SSE 连接的心跳流继续发送但不再收到业务消息，
            // 前端如果没有及时关闭旧连接，会看到孤立的心跳流
            let cleared_count = if let Some((_, old_session_data)) =
                SESSION_CACHE.remove(&old_session_id)
            {
                old_session_data.close_current_connection().await;
                info!(
                    "[ensure_project_session] Closed old session SSE connection: old_session_id={}",
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
                    "Cleared old session data and updated mapping: project_id={}, old_session_id={}, new_session_id={}, cleared_count={}",
                    project_id, old_session_id, session_id, cleared_count
                );
            } else {
                info!(
                    "Updated project session mapping: project_id={}, old_session_id={}, new_session_id={}",
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
                "Project session first seen: project_id={}, session_id={}",
                project_id, session_id
            );
            0
        }
    }
}

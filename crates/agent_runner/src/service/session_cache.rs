//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

#![allow(dead_code)]

use crate::service::PERMISSION_MANAGER;
use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use arc_swap::ArcSwapOption;
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
type SessionMessageSender = mpsc::Sender<(u64, UnifiedSessionMessage)>;

struct ConnectionState {
    sender: SessionMessageSender,
    cancel: CancellationToken,
}

/// 当前活跃 SSE 连接的不可变快照。消息热路径 lock-free 读取，重连/关闭原子替换整组状态。
type CurrentConnection = Arc<ArcSwapOption<ConnectionState>>;

pub struct SessionData {
    command_tx: mpsc::Sender<SessionCommand>,
    // 🎯 极简优化：直接存储当前连接，无需命令传递
    current_connection: CurrentConnection,
    // 🔒 Critical fix: 存储 worker JoinHandle，用于检测 panic
    worker_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// session 的 stream epoch(每次 new 生成新值)。rcoder 据此判断 seq epoch 是否变化:
    /// 进程重启 / SessionWorker panic 重建都会 new → 新 epoch → rcoder 重置 last_seq + 清 ring(#15)。
    epoch: String,
}

impl SessionData {
    /// 该 session 的 stream epoch(进程重启/worker panic 重建会换新)。供 rcoder 判断 seq epoch。
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

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
            current_connection: Arc::new(ArcSwapOption::empty()),
            worker_handle: Arc::new(tokio::sync::Mutex::new(None)),
            epoch: uuid::Uuid::now_v7().simple().to_string(),
        });
        debug!(
            "[SessionData::new] Arc creation took: {:?}",
            arc_start.elapsed()
        );

        let spawn_start = std::time::Instant::now();
        let handle = SessionWorker::spawn(
            max_size,
            command_rx,
            Arc::clone(&session.current_connection),
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

    /// 增量回放 ring buffer：只返回 seq > from_seq 的消息（非破坏性读取，buffer 内容不变）。
    ///
    /// 用于 SSE 连接建立时按订阅方的消费游标补齐缺失消息：
    /// - from_seq = 0：全量回放（首次订阅 / 向后兼容旧行为）。
    /// - from_seq = N：只补 N 之后的消息，已收过的不重发（消除重复）。
    pub async fn replay_since(&self, from_seq: u64) -> Vec<(u64, UnifiedSessionMessage)> {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::ReplaySince { from_seq, ack: tx })
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
                // 安全：持有 Mutex 锁期间 as_ref() 与 take() 之间无竞争
                let handle = match guard.take() {
                    Some(h) => h,
                    None => return false, // 锁内另一分支已消费
                };
                // JoinHandle 已从共享状态移出；等待它之前释放锁，避免健康检查等调用
                // 在任务退出清理较慢时被无谓串行化。
                drop(guard);
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
        from_seq: u64,
    ) -> Result<(
        Vec<(u64, UnifiedSessionMessage)>,
        mpsc::Receiver<(u64, UnifiedSessionMessage)>,
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
        let previous = self.current_connection.swap(Some(Arc::new(ConnectionState {
            sender: tx,
            cancel: cancellation_token.clone(),
        })));
        if let Some(previous) = previous {
            previous.cancel.cancel();
        }
        debug!(
            "[create_new_connection] Connection state setup took: {:?}",
            setup_start.elapsed()
        );

        // 📼 回放 ring buffer 中的历史消息（在设置 current_connection 之后）
        // 确保快照包含设置 current_connection 之前缓冲的所有消息
        // 时序保证：
        // 1. 设置 current_connection 后，新消息会通过 channel 发送
        // 2. replay_buffer() 获取的是设置 current_connection 之前缓冲的消息
        // 3. 这些消息不会通过 current_connection 发送，所以需要回放
        let replay_start = std::time::Instant::now();
        let replay_messages = self.replay_since(from_seq).await;
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
    pub fn close_current_connection(&self) {
        // 🎯 主动触发取消令牌，关闭 SSE 连接
        if let Some(connection) = self.current_connection.swap(None) {
            info!("[SessionData] Triggering CancellationToken to close SSE connection");
            connection.cancel.cancel();
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
    current_connection: CurrentConnection,
}

impl SessionWorker {
    fn spawn(
        max_size: usize,
        command_rx: mpsc::Receiver<SessionCommand>,
        current_connection: CurrentConnection,
    ) -> tokio::task::JoinHandle<()> {
        let start_time = std::time::Instant::now();
        debug!(
            "[SessionWorker::spawn] Starting SessionWorker creation, max_size={}",
            max_size
        );

        let worker = SessionWorker {
            max_size,
            command_rx,
            current_connection,
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
        // ring buffer 存 (seq, message)：seq 是 session 级单调递增的序号，
        // 供订阅方增量 replay 与去重（见 SessionCommand::ReplaySince）。
        let (mut producer, mut consumer) =
            HeapRb::<(u64, UnifiedSessionMessage)>::new(self.max_size).split();
        let mut buffered_len = 0usize;
        // session 级单调递增的消息序号。注意：Clear 命令清空 ring buffer 内容但【不重置】
        // next_seq——否则新一轮 prompt 的 seq 从 1 重来，会与订阅方持有的 last_seq 撞车导致漏发。
        let mut next_seq: u64 = 1;

        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                SessionCommand::Push { message } => {
                    debug!(
                        "[SessionWorker] Push message: message_type={:?}, sub_type={}, data={}",
                        message.message_type, message.sub_type, message.data
                    );

                    let should_buffer = !matches!(
                        message.message_type,
                        crate::model::SessionMessageType::Heartbeat
                    );

                    // 入 buffer 的消息分配 session 级单调 seq；Heartbeat 等不入 buffer 的消息用
                    // seq=0（哨兵：订阅方据此跳过去重，不更新消费游标）。
                    let seq = if should_buffer {
                        let s = next_seq;
                        next_seq += 1;
                        s
                    } else {
                        0
                    };

                    if should_buffer {
                        if producer.is_full() {
                            let _ = consumer.try_pop();
                            buffered_len = buffered_len.saturating_sub(1);
                        }
                        if producer.try_push((seq, message.clone())).is_ok() {
                            buffered_len += 1;
                        } else {
                            warn!("Ring buffer push failed; real-time delivery only");
                        }
                    }

                    let current_connection = self.current_connection.load();
                    if let Some(connection) = current_connection.as_ref() {
                        use tokio::sync::mpsc::error::TrySendError;
                        if let Err(send_err) = connection.sender.try_send((seq, message.clone())) {
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
                                    // 仅当它仍是当前连接时清除，避免旧 receiver 关闭误删刚建立的新连接。
                                    let _ = self
                                        .current_connection
                                        .compare_and_swap(&*current_connection, None);
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
                    // 仅清空 ring buffer 内容；next_seq 保持单调（见 run 开头注释），不随 clear 重置。
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
                SessionCommand::ReplaySince { from_seq, ack } => {
                    // 非破坏性读取：用 consumer.iter() 扫描，不 pop，ring buffer 内容与顺序不变。
                    // 只返回 seq > from_seq 的消息（增量补齐），消除"已收过的消息被重放"的重复。
                    let occupied = consumer.occupied_len();
                    let snapshot: Vec<(u64, UnifiedSessionMessage)> = consumer
                        .iter()
                        .filter(|(s, _)| *s > from_seq)
                        .cloned()
                        .collect();
                    debug!(
                        "[SessionWorker] ReplaySince: from_seq={}, matched {} of {} buffered messages (non-destructive)",
                        from_seq,
                        snapshot.len(),
                        occupied
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
    /// 增量 replay：只返回 ring buffer 中 seq > from_seq 的消息（非破坏性读取）。
    ReplaySince {
        from_seq: u64,
        ack: oneshot::Sender<Vec<(u64, UnifiedSessionMessage)>>,
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
            // 🛡️ 区分"合法正向迁移"与"过期 sessionId 反向顶替"。
            // 传入 sid 若不是当前 project 的活跃 session（已被新 session 顶替后从
            // session_to_project 移除 / 属于别的 project / 从未注册），说明是 agent
            // 用过期 sessionId 推来的迟到通知（如技能加载后的 available_commands_update）。
            // 此时只把消息 buffer 到该 sid，绝不反向改写 project 映射、不 cancel
            // 当前正在工作的真实 SSE（否则前端会收到 sub=cancelled）。
            let is_current_active = AGENT_REGISTRY
                .get_project_by_session(session_id)
                .map(|p| p == project_id)
                .unwrap_or(false);
            if !is_current_active {
                info!(
                    "Stale session_id ignored, skip migration (buffer-only): project_id={}, mapped={}, incoming={}",
                    project_id, old_session_id, session_id
                );
                return 0;
            }

            // 合法正向迁移（传入 sid 是当前 project 的活跃 session）：保留原有清理逻辑
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
                old_session_data.close_current_connection();
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

            // 清理旧 session_id 的动态权限状态（session_id 变更后旧 state 不再被引用，防泄漏）。
            PERMISSION_MANAGER.clear_session_state(&old_session_id);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionMessageType;
    use chrono::Utc;

    fn make_msg(sub_type: &str) -> UnifiedSessionMessage {
        UnifiedSessionMessage {
            session_id: "test-session".to_string(),
            message_type: SessionMessageType::AgentSessionUpdate,
            sub_type: sub_type.to_string(),
            data: serde_json::json!({}),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn replay_since_returns_only_messages_after_from_seq() {
        let sd = SessionData::new(64).await;
        sd.push_message(make_msg("a")); // seq 1
        sd.push_message(make_msg("b")); // seq 2
        sd.push_message(make_msg("c")); // seq 3

        let got: Vec<u64> = sd
            .replay_since(1)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(got, vec![2, 3], "replay_since(1) must return only seq>1");
    }

    #[tokio::test]
    async fn seq_keeps_monotonic_across_clear() {
        let sd = SessionData::new(64).await;
        sd.push_message(make_msg("a")); // seq 1
        sd.push_message(make_msg("b")); // seq 2
        let cleared = sd.clear_message_buffer().await;
        assert_eq!(cleared, 2);
        sd.push_message(make_msg("c")); // seq 必须为 3（不随 clear 重置）

        let got: Vec<u64> = sd
            .replay_since(0)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(got, vec![3], "seq must remain monotonic after clear");
    }

    #[tokio::test]
    async fn replay_since_is_non_destructive() {
        let sd = SessionData::new(64).await;
        sd.push_message(make_msg("a"));
        sd.push_message(make_msg("b"));

        let first: Vec<u64> = sd
            .replay_since(0)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        let second: Vec<u64> = sd
            .replay_since(0)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(first, second);
        assert_eq!(first, vec![1, 2], "replay must not drain the buffer");
    }

    fn make_heartbeat() -> UnifiedSessionMessage {
        UnifiedSessionMessage {
            session_id: "test-session".to_string(),
            message_type: SessionMessageType::Heartbeat,
            sub_type: "ping".to_string(),
            data: serde_json::json!({}),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn heartbeat_not_buffered_and_does_not_advance_seq() {
        let sd = SessionData::new(64).await;
        sd.push_message(make_heartbeat()); // Heartbeat：不入 ring，seq=0，不递增 next_seq
        sd.push_message(make_msg("a")); // seq 1
        sd.push_message(make_heartbeat());
        sd.push_message(make_msg("b")); // seq 2（Heartbeat 不占 seq 号）

        let got: Vec<u64> = sd
            .replay_since(0)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            got,
            vec![1, 2],
            "Heartbeat must not be buffered; seq must skip it"
        );
    }

    #[tokio::test]
    async fn ring_overflow_drops_oldest_and_keeps_seq_contiguous() {
        let sd = SessionData::new(3).await; // 容量 3
        sd.push_message(make_msg("a")); // seq 1
        sd.push_message(make_msg("b")); // seq 2
        sd.push_message(make_msg("c")); // seq 3
        sd.push_message(make_msg("d")); // seq 4，挤掉 seq1
        sd.push_message(make_msg("e")); // seq 5，挤掉 seq2

        let got: Vec<u64> = sd
            .replay_since(0)
            .await
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            got,
            vec![3, 4, 5],
            "ring overflow drops oldest, seq stays contiguous"
        );
    }

    #[tokio::test]
    async fn connection_swap_cancels_previous_and_close_drops_sender() {
        let session = SessionData::new(8).await;
        let (_, mut first_rx, first_cancel) = session
            .create_new_connection(8, 0)
            .await
            .expect("first connection");
        let (_, mut second_rx, second_cancel) = session
            .create_new_connection(8, 0)
            .await
            .expect("second connection");

        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(
            first_rx.recv().await.is_none(),
            "old sender must be dropped"
        );

        session.close_current_connection();
        assert!(second_cancel.is_cancelled());
        assert!(
            second_rx.recv().await.is_none(),
            "closing must atomically drop current sender"
        );
    }

    fn make_agent_info(project_id: &str, session_id: &str) -> shared_types::ProjectAndAgentInfo {
        use std::sync::Arc;
        use tokio::sync::mpsc;
        shared_types::ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: agent_client_protocol::schema::v1::SessionId::new(Arc::from(session_id)),
            prompt_tx: mpsc::channel(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY).0,
            cancel_tx: mpsc::channel(shared_types::AGENT_CANCEL_CHANNEL_CAPACITY).0,
            model_provider: None,
            request_id: None,
            status: shared_types::AgentStatus::Idle,
            last_activity: Utc::now(),
            created_at: Utc::now(),
            stop_handle: None,
            agent_binary_snapshot: None,
        }
    }

    #[tokio::test]
    async fn ensure_project_session_ignores_stale_session_id() {
        // C-slim 回归保护：agent 用过期/陌生 sessionId 推消息时，
        // 不得反向改写 project 映射、不得 cancel 当前正在工作的真实 SSE。
        let project = "cslim_stale_proj";
        let real_sid = "ses_real_active";
        let stale_sid = "753cf1fd-stale-not-registered";

        let registry = &crate::service::AGENT_REGISTRY;
        registry.remove_by_project(project); // 幂等清理残留
        registry.register(project, real_sid, make_agent_info(project, real_sid));

        // stale_sid 从未注册 → get_project_by_session(stale_sid)=None → 只 buffer，返回 0
        let cleared = ensure_project_session(project, stale_sid).await;
        assert_eq!(
            cleared, 0,
            "stale sid must be ignored (buffer-only, no migration)"
        );
        assert_eq!(
            registry.get_session_by_project(project).as_deref(),
            Some(real_sid),
            "active session mapping must NOT be overwritten by a stale sid"
        );

        registry.remove_by_project(project);
    }
}

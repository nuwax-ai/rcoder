//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

#![allow(dead_code)]

use crate::service::PERMISSION_MANAGER;
use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// worker 轻量命令应答超时（秒）: message_count / clear 等。
///
/// worker 被长任务占住时, 健康检查 / stop 等调用方应超时返回默认值
/// 而不是无限挂死, 避免拖死整个请求链路 (Fail Fast)。
const WORKER_REPLY_TIMEOUT_SECS: u64 = 5;

/// replay 类命令应答超时（秒）。
///
/// replay_since 用于 SSE/gRPC 重连补发历史消息; 它超时静默返回空会丢失
/// 重连区间消息, 违背 ring buffer 的设计初衷, 故给更长预算 (worker 被
/// 积压 Push 占住时宁可多等, 也不能静默丢消息)。
const WORKER_REPLAY_TIMEOUT_SECS: u64 = 30;

/// 带超时等待 worker 应答; 超时/worker 退出时返回默认值并告警。
async fn await_worker_reply<T: Default>(
    command: &str,
    timeout_secs: u64,
    rx: oneshot::Receiver<T>,
) -> T {
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            // RecvError: worker 侧 sender 已 drop (worker 退出/panic), 无更多细节
            warn!(
                %error,
                "Worker reply channel closed for {command}; worker has exited"
            );
            T::default()
        }
        Err(error) => {
            // Elapsed: 超时标记, 携带命令名 + 实际超时值便于定位慢点
            warn!(
                %error,
                "Worker reply timed out after {timeout_secs}s for {command}; returning default"
            );
            T::default()
        }
    }
}

/// Session数据包装 - 极简版本，专注消息传输
type SessionMessageSender = mpsc::Sender<(u64, UnifiedSessionMessage)>;

struct ConnectionState {
    sender: SessionMessageSender,
    cancel: CancellationToken,
}

/// 并发订阅者注册表（P2-M2 多订阅者改造）。
///
/// 历史实现是单槽 `ArcSwapOption`：新订阅者 swap 替换并 cancel 旧流
/// （last-writer-wins）——单副本 rcoder 只有一条共享流时正确；多副本/多端同看
/// 同一 session 时互相踢成死循环。改为 `conn_id → ConnectionState` 注册表：
/// 新订阅不再取消既有流，各自持独立 channel/token，Push 遍历投递；
/// 超过 [`MAX_SUBSCRIBERS`] 逐最旧（conn_id 单调，最小 id 即最旧）。
///
/// conn_id 由 SessionData 侧 AtomicU64 分配（session 内唯一）。
type ConnectionRegistry = Arc<DashMap<u64, ConnectionState>>;

/// 并发订阅者上限（多端同看场景的实际边界；超限逐最旧防滥用）
const MAX_SUBSCRIBERS: usize = 8;

pub struct SessionData {
    command_tx: mpsc::Sender<SessionCommand>,
    // 🎯 多订阅者注册表（见 ConnectionRegistry 文档）+ 单调 conn_id 分配器
    connections: ConnectionRegistry,
    next_conn_id: Arc<AtomicU64>,
    // 🔒 Critical fix: 存储 worker JoinHandle，用于检测 panic
    worker_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// session 的 stream epoch(每次 new 生成新值)。rcoder 据此判断 seq epoch 是否变化:
    /// 进程重启 / SessionWorker panic 重建都会 new → 新 epoch → rcoder 重置 last_seq + 清 ring(#15)。
    epoch: String,
    /// worker 已结束（panic/正常退出/cancel）标记。置位后并发调用据不再向已死 channel 推消息，
    /// 直接走重建路径（修复 has_worker_panicked 的 take→replace 竞态丢消息）。
    poisoned: AtomicBool,
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
            connections: Arc::new(DashMap::new()),
            next_conn_id: Arc::new(AtomicU64::new(1)),
            worker_handle: Arc::new(tokio::sync::Mutex::new(None)),
            epoch: uuid::Uuid::now_v7().simple().to_string(),
            poisoned: AtomicBool::new(false),
        });
        debug!(
            "[SessionData::new] Arc creation took: {:?}",
            arc_start.elapsed()
        );

        let spawn_start = std::time::Instant::now();
        let handle = SessionWorker::spawn(
            max_size,
            command_rx,
            Arc::clone(&session.connections),
            Arc::clone(&session.next_conn_id),
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
        await_worker_reply("message_count", WORKER_REPLY_TIMEOUT_SECS, rx).await
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
        await_worker_reply("replay_since", WORKER_REPLAY_TIMEOUT_SECS, rx).await
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
        await_worker_reply("clear_message_buffer", WORKER_REPLY_TIMEOUT_SECS, rx).await
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
                // worker 已结束（panic/正常退出/cancel），channel 随之关闭。先标记 poisoned，
                // 使并发调用（handle 已被 take 走、看到 None）据此重建，不再向已死 channel 推消息。
                self.poisoned.store(true, Ordering::Release);
                // take() 消耗 handle 来 await 获取结果
                // 安全：持有 Mutex 锁期间 as_ref() 与 take() 之间无竞争
                let handle = match guard.take() {
                    Some(h) => h,
                    None => return self.poisoned.load(Ordering::Acquire),
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
            // handle 已被并发调用 take 走（worker 已结束）：据 poisoned 判断是否需重建
            self.poisoned.load(Ordering::Acquire)
        }
    }

    /// 建立新订阅连接。返回 `(conn_id, 回放消息, receiver, cancel_token)`；
    /// conn_id 用于流结束时 [`SessionData::close_connection`] 精确关闭自己
    /// （不误伤其他订阅者）。
    pub async fn create_new_connection(
        &self,
        buffer_size: usize,
        from_seq: u64,
    ) -> Result<(
        u64,
        Vec<(u64, UnifiedSessionMessage)>,
        mpsc::Receiver<(u64, UnifiedSessionMessage)>,
        CancellationToken,
    )> {
        // 原子化订阅：快照与 sender 注册在 worker 单线程内完成（SessionCommand::Subscribe），
        // 消除旧实现「先注册后快照」窗口内消息既进 ring 又进 sender 的双路投递
        // （gRPC 流重复 → rcoder ring 重复条目 → 无游标重连吐给客户端的根因）。
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .command_tx
            .send(SessionCommand::Subscribe {
                buffer_size,
                from_seq,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            anyhow::bail!("worker has exited");
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(WORKER_REPLY_TIMEOUT_SECS),
            ack_rx,
        )
        .await
        {
            Ok(Ok(r)) => Ok((r.conn_id, r.replay_messages, r.rx, r.token)),
            Ok(Err(_)) => anyhow::bail!("worker dropped subscribe ack"),
            Err(_) => anyhow::bail!("subscribe ack timeout ({WORKER_REPLY_TIMEOUT_SECS}s)"),
        }
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

    /// 当前订阅者数（测试/诊断用）
    pub fn connections_len(&self) -> usize {
        self.connections.len()
    }

    /// 关闭指定订阅连接（流结束/断开时调用；只关自己，不误伤其他订阅者）
    pub fn close_connection(&self, conn_id: u64) {
        if let Some((_, connection)) = self.connections.remove(&conn_id) {
            info!(
                "[SessionData] Closing subscriber {conn_id}: CancellationToken triggered, sender dropped"
            );
            connection.cancel.cancel();
        }
    }

    /// 关闭全部订阅连接（任务取消 / 会话停止 / 新 prompt 前清场等
    /// "终结所有观看端"语义的场景）
    pub fn close_all_connections(&self) {
        let count = self.connections.len();
        for entry in self.connections.iter() {
            entry.value().cancel.cancel();
        }
        self.connections.clear();
        if count > 0 {
            info!("[SessionData] Closed all {count} SSE subscriber connection(s)");
        }
    }
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
                old_session_data.close_all_connections();
                info!(
                    "[ensure_project_session] Closed old session SSE connection: old_session_id={}",
                    old_session_id
                );
                1 // 移除了1个session
            } else {
                0 // session不存在
            };

            // 更新 AGENT_REGISTRY 中的映射关系
            drop(AGENT_REGISTRY.update_session(project_id, session_id));

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

#[cfg(test)]
mod tests;
mod worker;

use worker::SessionCommand;
pub(crate) use worker::SessionWorker;

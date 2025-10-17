//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use dashmap::DashMap;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use std::sync::{Arc, LazyLock, atomic::{AtomicBool, AtomicU64, Ordering}};

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, Arc<SessionData>>> = LazyLock::new(|| DashMap::new());

/// Project到当前活跃Session的映射 - 用于确保一个project_id只对应一个session_id
/// 当project_id对应的session_id发生变化时，会自动清理旧session的数据
pub static PROJECT_SESSION_MAP: LazyLock<DashMap<String, String>> = LazyLock::new(|| DashMap::new());


/// Session数据包装
pub struct SessionData {
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    is_cancelled: AtomicBool,
    version: AtomicU64,
}

impl SessionData {
    pub fn new(max_size: usize) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let session = Arc::new(SessionData {
            command_tx,
            is_cancelled: AtomicBool::new(false),
            version: AtomicU64::new(0),
        });

        SessionWorker::spawn(max_size, command_rx, Arc::downgrade(&session));

        session
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Acquire)
    }

    pub fn set_cancelled(&self, cancelled: bool) {
        self.is_cancelled.store(cancelled, Ordering::Release);
        let _ = self.command_tx.send(SessionCommand::SetCancelled(cancelled));
    }

    pub async fn clear_messages(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(SessionCommand::Clear { ack: tx }).is_err() {
            warn!("⚠️ clear_messages 指令发送失败，worker 已退出");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub async fn clear_all(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(SessionCommand::ClearAll { ack: tx }).is_err() {
            warn!("⚠️ clear_all 指令发送失败，worker 已退出");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub async fn create_new_connection(
        &self,
        buffer_size: usize,
    ) -> Result<(mpsc::Receiver<UnifiedSessionMessage>, CancellationToken, u64)> {
        let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        let cancellation_token = CancellationToken::new();
        let (tx, rx) = mpsc::channel(buffer_size);
        let (ack_tx, ack_rx) = oneshot::channel();

        let command = SessionCommand::Register {
            sender: tx,
            cancel_token: cancellation_token.clone(),
            version,
            ack: ack_tx,
        };

        self.is_cancelled.store(false, Ordering::Release);

        self
            .command_tx
            .send(command)
            .map_err(|err| anyhow::anyhow!("发送注册指令失败: {}", err))?;

        ack_rx
            .await
            .map_err(|err| anyhow::anyhow!("等待注册响应失败: {}", err))?;

        Ok((rx, cancellation_token, version))
    }

    pub fn push_message(&self, message: UnifiedSessionMessage) {
        if self
            .command_tx
            .send(SessionCommand::Push { message })
            .is_err()
        {
            warn!("⚠️ 推送消息失败，worker 已退出");
        }
    }

    pub async fn message_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(SessionCommand::MessageCount { ack: tx }).is_err() {
            warn!("⚠️ message_count 指令发送失败，worker 已退出");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub fn current_version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

struct SessionWorker {
    max_size: usize,
    command_rx: mpsc::UnboundedReceiver<SessionCommand>,
    session: std::sync::Weak<SessionData>,
}

impl SessionWorker {
    fn spawn(
        max_size: usize,
        command_rx: mpsc::UnboundedReceiver<SessionCommand>,
        session: std::sync::Weak<SessionData>,
    ) {
        let worker = SessionWorker {
            max_size,
            command_rx,
            session,
        };
        tokio::spawn(worker.run());
    }

    async fn run(mut self) {
        let (mut producer, mut consumer) = HeapRb::new(self.max_size).split();
        let mut buffered_len = 0usize;
        let mut current_sender: Option<mpsc::Sender<UnifiedSessionMessage>> = None;
        let mut current_cancel: Option<CancellationToken> = None;

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
                            warn!("⚠️ ring buffer push 失败，只能实时推送");
                        }
                    }

                    if let Some(sender) = current_sender.as_mut() {
                        if sender.send(message).await.is_err() {
                            warn!("⚠️ SSE sender 已关闭，丢弃实时推送");
                            current_sender = None;
                        }
                    }
                }
                SessionCommand::Register {
                    sender,
                    cancel_token,
                    version,
                    ack,
                } => {
                    if let Some(token) = current_cancel.take() {
                        token.cancel();
                    }

                    current_sender = Some(sender.clone());
                    current_cancel = Some(cancel_token);

                    let mut drained = Vec::new();
                    while let Some(msg) = consumer.try_pop() {
                        drained.push(msg);
                    }
                    buffered_len = 0;

                    for msg in drained {
                        if sender.send(msg.clone()).await.is_err() {
                            warn!("⚠️ 历史消息发送失败，SSE sender 已关闭");
                            current_sender = None;
                            break;
                        }
                    }

                    let _ = ack.send(());

                    if let Some(session) = self.session.upgrade() {
                        session.version.store(version, Ordering::Release);
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
                SessionCommand::ClearAll { ack } => {
                    if let Some(token) = current_cancel.take() {
                        token.cancel();
                    }
                    current_sender = None;

                    let mut cleared = 0usize;
                    while consumer.try_pop().is_some() {
                        cleared += 1;
                    }
                    buffered_len = 0;
                    let _ = ack.send(cleared);
                }
                SessionCommand::SetCancelled(cancelled) => {
                    if cancelled {
                        if let Some(token) = current_cancel.take() {
                            token.cancel();
                        }
                        current_sender = None;
                    }
                }
                SessionCommand::MessageCount { ack } => {
                    let _ = ack.send(buffered_len);
                }
            }
        }

        debug!("🔚 SessionWorker 结束运行");
    }
}

#[derive(Debug)]
enum SessionCommand {
    Push { message: UnifiedSessionMessage },
    Register {
        sender: mpsc::Sender<UnifiedSessionMessage>,
        cancel_token: CancellationToken,
        version: u64,
        ack: oneshot::Sender<()>,
    },
    Clear { ack: oneshot::Sender<usize> },
    ClearAll { ack: oneshot::Sender<usize> },
    SetCancelled(bool),
    MessageCount { ack: oneshot::Sender<usize> },
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    let session_data = SESSION_CACHE
        .entry(session_id.to_string())
        .or_insert_with(|| SessionData::new(1000))
        .clone();

    if session_data.is_cancelled() {
        debug!("🚫 Session已取消，丢弃消息: session_id={}", session_id);
        return Ok(());
    }

    let unified_message = notify.to_unified_message();

    debug!(
        "📥 推送消息到缓存: session_id={}, message_type={:?}, sub_type={}",
        session_id,
        unified_message.message_type,
        unified_message.sub_type
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
pub async fn push_session_update_with_project(project_id: &str, session_id: &str, notify: SessionNotify) -> Result<()> {
    // 确保project_id对应正确的session_id，如果变化则清理旧数据
    let cleared_count = ensure_project_session(project_id, session_id).await;

    if cleared_count > 0 {
        info!(
            "📝 [push_session_update_with_project] 检测到session变化，已清理 {} 条旧消息: project_id={}, new_session_id={}",
            cleared_count, project_id, session_id
        );
    }

    // 推送消息到新的session
    push_session_update(session_id, notify).await
}

/// 便捷函数：清空指定 session_id 的所有消息（用于取消任务时避免历史消息积压）
pub async fn clear_session_messages(session_id: &str) -> usize {
    if let Some(session_data_ref) = SESSION_CACHE.get(session_id) {
        let session_data = session_data_ref.clone();
        drop(session_data_ref);

        let cleared_count = session_data.clear_messages().await;
        info!(
            "🧹 清空 SSE 消息缓存: session_id={}, cleared_count={}",
            session_id,
            cleared_count
        );
        cleared_count
    } else {
        info!(
            "⚠️ 试图清空不存在的 session 消息: session_id={}",
            session_id
        );
        0
    }
}

/// 便捷函数：清空指定 project_id 的所有 session 消息
///
/// 这个函数会遍历所有 session，找到属于指定 project_id 的 session 并清空其消息
/// 主要用于：
/// 1. 发起新对话时清空历史消息（/chat 接口）
/// 2. 取消任务时清空历史消息（/agent/session/cancel 接口）
/// 3. 停止服务时清空历史消息（/agent/stop 接口）
///
/// 确保前端SSE连接获取的都是当前对话触发的最新消息，避免历史消息干扰
pub async fn clear_project_messages(
    project_id: &str,
    sessions_map: &dashmap::DashMap<String, crate::router::SessionInfo>,
    specific_session: Option<&str>,
) -> usize {
    let mut total_cleared = 0;
    let mut specific_session_handled = false;

    // 遍历所有活跃的 session，找到属于指定 project_id 的 session
    for session_entry in sessions_map.iter() {
        let session_id = session_entry.key();
        let session_info = session_entry.value();

        // 检查这个 session 是否属于指定的 project_id
        if let Some(session_project_id) = &session_info.project_id {
            if session_project_id == project_id {
                if let Some(target_session) = specific_session {
                    if target_session == session_id {
                        specific_session_handled = true;
                    }
                }
                // 清空这个 session 的消息
                let cleared_count = clear_session_messages(session_id).await;
                total_cleared += cleared_count;

                if cleared_count > 0 {
                    debug!(
                        "🧹 清空项目消息: project_id={}, session_id={}, cleared_count={}",
                        project_id, session_id, cleared_count
                    );
                }

                if SESSION_CACHE.remove(session_id).is_some() {
                    info!(
                        "🧼 移除 SESSION_CACHE 条目: project_id={}, session_id={}",
                        project_id,
                        session_id
                    );
                }
            }
        }
    }

    if let Some(target_session) = specific_session {
        if !specific_session_handled {
            let cleared_count = clear_session_messages(target_session).await;
            if cleared_count > 0 {
                info!(
                    "🧹 针对指定 session 追加清理: project_id={}, session_id={}, cleared_count={}",
                    project_id,
                    target_session,
                    cleared_count
                );
                total_cleared += cleared_count;
            }

            if SESSION_CACHE.remove(target_session).is_some() {
                info!(
                    "🧼 移除指定 SESSION_CACHE 条目: project_id={}, session_id={}",
                    project_id,
                    target_session
                );
            }
        }
    }

    if total_cleared > 0 {
        info!(
            "📝 项目消息清空完成: project_id={}, total_cleared={}, sessions_found={}",
            project_id, total_cleared, sessions_map.len()
        );
    } else {
        debug!(
            "📭 项目无历史消息需要清空: project_id={}",
            project_id
        );
    }

    total_cleared
}

/// 确保project_id对应正确的session_id
///
/// 如果project_id对应的session_id发生变化，会自动清理旧session的数据
/// 如果session_id相同，则不做任何操作
///
/// 参数:
/// - project_id: 项目ID
/// - session_id: 当前会话ID
///
/// 返回值: 如果清理了旧数据则返回清理的消息数量，否则返回0
pub async fn ensure_project_session(project_id: &str, session_id: &str) -> usize {
    // 检查当前映射
    let mapped_session_id = if let Some(entry) = PROJECT_SESSION_MAP.get(project_id) {
        let mapped_session_id = entry.value().clone();
        
        // 如果session_id相同，不需要做任何操作
        if mapped_session_id == session_id {
            debug!(
                "📋 Project session映射未变化: project_id={}, session_id={}",
                project_id, session_id
            );
            return 0;
        }
        
        // ⚠️ 关键修复：显式 drop entry，释放读锁，避免后续 insert 时的读写锁冲突
        drop(entry);
        
        Some(mapped_session_id)
    } else {
        None
    };
    
    // 如果有旧映射，处理 session 变化
    if let Some(mapped_session_id) = mapped_session_id {
        // session_id发生变化，需要清理旧session的数据
        info!(
            "🔄 检测到Project session变化: project_id={}, old_session_id={}, new_session_id={}",
            project_id, mapped_session_id, session_id
        );

        // 清理旧session的数据
        let cleared_count = clear_session_messages(&mapped_session_id).await;

        // 更新映射关系（现在可以安全获取写锁）
        PROJECT_SESSION_MAP.insert(project_id.to_string(), session_id.to_string());

        if cleared_count > 0 {
            info!(
                "🧹 已清理旧session数据并更新映射: project_id={}, old_session_id={}, new_session_id={}, cleared_count={}",
                project_id, mapped_session_id, session_id, cleared_count
            );
        } else {
            info!(
                "📝 已更新Project session映射: project_id={}, old_session_id={}, new_session_id={}",
                project_id, mapped_session_id, session_id
            );
        }

        cleared_count
    } else {
        // 第一次建立映射关系（无旧映射）
        info!(
            "🆕 建立新的Project session映射: project_id={}, session_id={}",
            project_id, session_id
        );
        PROJECT_SESSION_MAP.insert(project_id.to_string(), session_id.to_string());
        0
    }
}

/// 获取project_id当前对应的session_id
///
/// 如果映射不存在则返回None
pub fn get_project_session(project_id: &str) -> Option<String> {
    PROJECT_SESSION_MAP.get(project_id).map(|session_id| session_id.clone())
}

/// 移除project_id的session映射（用于项目清理）
///
/// 返回被移除的session_id，如果映射不存在则返回None
pub fn remove_project_session(project_id: &str) -> Option<String> {
    if let Some((_, session_id)) = PROJECT_SESSION_MAP.remove(project_id) {
        info!(
            "🗑️ 移除Project session映射: project_id={}, session_id={}",
            project_id, session_id
        );
        Some(session_id)
    } else {
        debug!(
            "⚠️ 试图移除不存在的Project session映射: project_id={}",
            project_id
        );
        None
    }
}

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
use std::sync::{Arc, LazyLock};

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, Arc<SessionData>>> = LazyLock::new(DashMap::new);

/// Project到当前活跃Session的映射 - 用于确保一个project_id只对应一个session_id
/// 当project_id对应的session_id发生变化时，会自动清理旧session的数据
pub static PROJECT_SESSION_MAP: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);


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
        debug!("⏱️ [SessionData::new] 开始创建，max_size={}", max_size);

        let channel_start = std::time::Instant::now();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        debug!("⏱️ [SessionData::new] channel创建耗时: {:?}", channel_start.elapsed());

        let arc_start = std::time::Instant::now();
        let session = Arc::new(SessionData {
            command_tx,
            current_sender: Arc::new(tokio::sync::Mutex::new(None)),
            current_cancel: Arc::new(tokio::sync::Mutex::new(None)),
        });
        debug!("⏱️ [SessionData::new] Arc创建耗时: {:?}", arc_start.elapsed());

        let spawn_start = std::time::Instant::now();
        SessionWorker::spawn(max_size, command_rx, session.current_sender.clone(), session.current_cancel.clone());
        debug!("⏱️ [SessionData::new] SessionWorker::spawn耗时: {:?}", spawn_start.elapsed());

        debug!("⏱️ [SessionData::new] 总创建耗时: {:?}", start_time.elapsed());
        session
    }

  
    pub async fn message_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(SessionCommand::MessageCount { ack: tx }).is_err() {
            warn!("⚠️ message_count 指令发送失败，worker 已退出");
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub async fn create_new_connection(
        &self,
        buffer_size: usize,
    ) -> Result<(mpsc::Receiver<UnifiedSessionMessage>, CancellationToken)> {
        let start_time = std::time::Instant::now();
        debug!("⏱️ [create_new_connection] 开始创建连接，buffer_size={}", buffer_size);

        let token_start = std::time::Instant::now();
        let cancellation_token = CancellationToken::new();
        debug!("⏱️ [create_new_connection] CancellationToken创建耗时: {:?}", token_start.elapsed());

        let channel_start = std::time::Instant::now();
        let (tx, rx) = mpsc::channel(buffer_size);
        debug!("⏱️ [create_new_connection] mpsc channel创建耗时: {:?}", channel_start.elapsed());

        let setup_start = std::time::Instant::now();
        // 🎯 极简优化：直接设置连接状态，无需命令传递
        {
            // 取消之前的连接
            if let Ok(mut current_cancel_guard) = self.current_cancel.try_lock() {
                if let Some(token) = current_cancel_guard.take() {
                    token.cancel();
                }
                // 设置新的取消令牌
                *current_cancel_guard = Some(cancellation_token.clone());
            }

            // 设置新的发送器
            if let Ok(mut current_sender_guard) = self.current_sender.try_lock() {
                *current_sender_guard = Some(tx.clone());
            }
        }
        debug!("⏱️ [create_new_connection] 连接状态设置耗时: {:?}", setup_start.elapsed());

        debug!("⏱️ [create_new_connection] 总连接创建耗时: {:?}", start_time.elapsed());
        Ok((rx, cancellation_token))
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
        if let Ok(mut current_cancel_guard) = self.current_cancel.try_lock()
            && let Some(token) = current_cancel_guard.take() {
                info!("🔌 [SessionData] 主动触发CancellationToken，关闭SSE连接");
                token.cancel();
            }

        // 🎯 显式关闭 channel 发送端，让接收端立即感知到连接关闭
        if let Ok(mut current_sender_guard) = self.current_sender.try_lock()
            && let Some(_sender) = current_sender_guard.take() {
                info!("🔌 [SessionData] 显式关闭channel发送端，让接收端立即断开");
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
        debug!("⏱️ [SessionWorker::spawn] 开始创建SessionWorker，max_size={}", max_size);

        let worker = SessionWorker {
            max_size,
            command_rx,
            current_sender,
            current_cancel,
        };

        let spawn_start = std::time::Instant::now();
        tokio::spawn(worker.run());
        debug!("⏱️ [SessionWorker::spawn] tokio::spawn耗时: {:?}", spawn_start.elapsed());
        debug!("⏱️ [SessionWorker::spawn] 总spawn耗时: {:?}", start_time.elapsed());
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
                            warn!("⚠️ ring buffer push 失败，只能实时推送");
                        }
                    }

                    // 🚀 极简优化：直接从共享状态获取当前连接
                    if let Ok(mut current_sender_guard) = self.current_sender.try_lock()
                        && let Some(sender) = current_sender_guard.as_mut()
                            && sender.try_send(message.clone()).is_err() {
                                // 如果发送失败，可能是缓冲区满了或连接已关闭
                                warn!("⚠️ SSE sender 发送失败，关闭实时推送");
                                *current_sender_guard = None;
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

        debug!("🔚 SessionWorker 结束运行");
    }
}

#[derive(Debug)]
enum SessionCommand {
    Push { message: UnifiedSessionMessage },
    Clear { ack: oneshot::Sender<usize> },
    MessageCount { ack: oneshot::Sender<usize> },
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    // 🎯 极简设计：直接获取当前 SESSION_CACHE 中的 SessionData
    // Agent 只能发送到最新创建的 SessionData，确保消息路由清晰
    let session_data = if let Some(session_data_ref) = SESSION_CACHE.get(session_id) {
        session_data_ref.clone()
    } else {
        debug!(
            "🚫 [push_session_update] session={} 不存在于 SESSION_CACHE 中，可能是 SSE 连接未建立",
            session_id
        );
        return Ok(());
    };

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

        // 清理旧session的数据 - 直接移除条目
        let cleared_count = if SESSION_CACHE.remove(&mapped_session_id).is_some() {
            1 // 移除了1个session
        } else {
            0 // session不存在
        };

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

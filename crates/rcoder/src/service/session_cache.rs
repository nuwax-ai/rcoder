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


/// Session数据包装 - 保留核心功能的简化版
pub struct SessionData {
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    is_cancelled: AtomicBool,
    version: AtomicU64,
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
            is_cancelled: AtomicBool::new(false),
            version: AtomicU64::new(0),
        });
        debug!("⏱️ [SessionData::new] Arc创建耗时: {:?}", arc_start.elapsed());

        let spawn_start = std::time::Instant::now();
        SessionWorker::spawn(max_size, command_rx, Arc::downgrade(&session));
        debug!("⏱️ [SessionData::new] SessionWorker::spawn耗时: {:?}", spawn_start.elapsed());

        debug!("⏱️ [SessionData::new] 总创建耗时: {:?}", start_time.elapsed());
        session
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Acquire)
    }

    pub fn set_cancelled(&self, cancelled: bool) {
        self.is_cancelled.store(cancelled, Ordering::Release);
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
    ) -> Result<(mpsc::Receiver<UnifiedSessionMessage>, CancellationToken, u64)> {
        let start_time = std::time::Instant::now();
        debug!("⏱️ [create_new_connection] 开始创建连接，buffer_size={}", buffer_size);

        let token_start = std::time::Instant::now();
        let cancellation_token = CancellationToken::new();
        debug!("⏱️ [create_new_connection] CancellationToken创建耗时: {:?}", token_start.elapsed());

        let channel_start = std::time::Instant::now();
        let (tx, rx) = mpsc::channel(buffer_size);
        let (ack_tx, ack_rx) = oneshot::channel();
        debug!("⏱️ [create_new_connection] mpsc channel创建耗时: {:?}", channel_start.elapsed());

        let command_start = std::time::Instant::now();
        let command = SessionCommand::Register {
            sender: tx,
            cancel_token: cancellation_token.clone(),
            ack: ack_tx,
        };
        debug!("⏱️ [create_new_connection] command创建耗时: {:?}", command_start.elapsed());

        let send_start = std::time::Instant::now();
        self
            .command_tx
            .send(command)
            .map_err(|err| anyhow::anyhow!("发送注册指令失败: {}", err))?;
        debug!("⏱️ [create_new_connection] command_tx.send耗时: {:?}", send_start.elapsed());

        let wait_start = std::time::Instant::now();
        let version = ack_rx
            .await
            .map_err(|err| anyhow::anyhow!("等待注册响应失败: {}", err))?;
        debug!("⏱️ [create_new_connection] ack_rx.await耗时: {:?}", wait_start.elapsed());

        debug!("⏱️ [create_new_connection] 总连接创建耗时: {:?}", start_time.elapsed());
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
        let start_time = std::time::Instant::now();
        debug!("⏱️ [SessionWorker::spawn] 开始创建SessionWorker，max_size={}", max_size);

        let worker = SessionWorker {
            max_size,
            command_rx,
            session,
        };

        let spawn_start = std::time::Instant::now();
        tokio::spawn(worker.run());
        debug!("⏱️ [SessionWorker::spawn] tokio::spawn耗时: {:?}", spawn_start.elapsed());
        debug!("⏱️ [SessionWorker::spawn] 总spawn耗时: {:?}", start_time.elapsed());
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

                    // 🚀 使用非阻塞发送，避免阻塞其他命令
                    if let Some(sender) = current_sender.as_mut() {
                        if sender.try_send(message.clone()).is_err() {
                            // 如果发送失败，可能是缓冲区满了或连接已关闭
                            warn!("⚠️ SSE sender 发送失败，关闭实时推送");
                            current_sender = None;
                        }
                    }
                }
                SessionCommand::Register {
                    sender,
                    cancel_token,
                    ack,
                } => {
                    let register_start = std::time::Instant::now();
                    debug!("⏱️ [SessionWorker::Register] 开始处理注册命令");

                    if let Some(token) = current_cancel.take() {
                        token.cancel();
                    }

                    current_sender = Some(sender.clone());
                    current_cancel = Some(cancel_token);

                    // 🎯 原子化版本号递增：在 SessionWorker 中递增，确保顺序一致性
                    let version_start = std::time::Instant::now();
                    let version = if let Some(session) = self.session.upgrade() {
                        session.version.fetch_add(1, Ordering::SeqCst) + 1
                    } else {
                        0
                    };
                    debug!("⏱️ [SessionWorker::Register] 版本号递增耗时: {:?}", version_start.elapsed());

                    // 🎯 简化的历史消息处理：清空所有历史消息，确保全新开始
                    let clear_start = std::time::Instant::now();
                    let mut cleared_count = 0;
                    while consumer.try_pop().is_some() {
                        cleared_count += 1;
                    }
                    buffered_len = 0;
                    debug!("⏱️ [SessionWorker::Register] 清空历史消息耗时: {:?}，清理了{}条", clear_start.elapsed(), cleared_count);

                    if cleared_count > 0 {
                        debug!("🧹 新SSE连接(v={})，清空 {} 条历史消息", version, cleared_count);
                    }

                    // 返回版本号给调用者
                    let ack_start = std::time::Instant::now();
                    let _ = ack.send(version);
                    debug!("⏱️ [SessionWorker::Register] ack.send耗时: {:?}", ack_start.elapsed());

                    debug!("⏱️ [SessionWorker::Register] 总注册处理耗时: {:?}", register_start.elapsed());
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
    Register {
        sender: mpsc::Sender<UnifiedSessionMessage>,
        cancel_token: CancellationToken,
        ack: oneshot::Sender<u64>, // 返回版本号
    },
    Clear { ack: oneshot::Sender<usize> },
    MessageCount { ack: oneshot::Sender<usize> },
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    // 🎯 关键修复：直接获取当前 SESSION_CACHE 中的 SessionData，不自动创建新的
    // 这样确保 Agent 使用的是 SSE 连接创建的最新 SessionData
    let session_data = if let Some(session_data_ref) = SESSION_CACHE.get(session_id) {
        session_data_ref.clone()
    } else {
        debug!(
            "🚫 [push_session_update] session={} 不存在于 SESSION_CACHE 中，可能是 SSE 连接未建立",
            session_id
        );
        return Ok(());
    };

    // 🎯 源过滤逻辑：检查session是否已被用户取消且没有新的聊天请求
    // 如果session已被取消且没有新的活跃请求，则忽略消息，防止残留消息
    if session_data.is_cancelled() {
        // 检查是否有活跃的SSE连接（表示有新的聊天请求）
        let message_count = session_data.message_count().await;
        if message_count == 0 {
            // session已被取消且没有活跃消息，说明没有新的聊天请求，忽略消息
            info!(
                "🚫 [push_session_update] session={} 已被取消且无活跃请求，忽略消息，防止残留",
                session_id
            );
            return Ok(());
        } else {
            debug!(
                "📝 [push_session_update] session={} 已被取消但有活跃消息({}条)，可能存在新请求，继续处理消息",
                session_id, message_count
            );
        }
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


/// 便捷函数：清空指定 project_id 的所有 session 消息
///
/// 这个函数会遍历所有 session，找到属于指定 project_id 的 session 并彻底清空其消息
/// 主要用于：
/// 1. 发起新对话时清空历史消息（/chat 接口）
/// 2. 取消任务时清空历史消息（/agent/session/cancel 接口）
/// 3. 停止服务时清空历史消息（/agent/stop 接口）
///
/// 🎯 彻底清空机制：
/// - 调用 clear_all() 清空 session 内部所有缓存消息
/// - 移除整个 SESSION_CACHE 条目，确保下次连接时创建全新的实例
/// - 防止任何历史消息残留到新的 SSE 连接中
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

                // 🎯 彻底清空机制：确保完全清除历史消息
                let cleared_count = clear_session_completely(session_id).await;
                total_cleared += cleared_count;

                if cleared_count > 0 {
                    info!(
                        "🧹 彻底清空项目消息: project_id={}, session_id={}, cleared_count={}",
                        project_id, session_id, cleared_count
                    );
                }
            }
        }
    }

    if let Some(target_session) = specific_session {
        if !specific_session_handled {
            // 🎯 对指定 session 进行彻底清空
            let cleared_count = clear_session_completely(target_session).await;
            if cleared_count > 0 {
                info!(
                    "🧹 彻底清空指定 session: project_id={}, session_id={}, cleared_count={}",
                    project_id,
                    target_session,
                    cleared_count
                );
                total_cleared += cleared_count;
            }
        }
    }

    if total_cleared > 0 {
        info!(
            "📝 项目消息彻底清空完成: project_id={}, total_cleared={}, sessions_found={}",
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

/// 彻底清空指定 session_id 的所有消息和缓存
///
/// 这个函数执行彻底的清空操作：
/// 1. 移除整个 SESSION_CACHE 条目，确保下次连接时创建全新的实例
/// 2. 防止任何历史消息残留到新的 SSE 连接中
///
/// 返回总共清理的消息数量（这里简化为固定返回1，表示移除了1个session）
async fn clear_session_completely(session_id: &str) -> usize {
    // 移除整个 SESSION_CACHE 条目，确保下次连接时创建全新的实例
    if SESSION_CACHE.remove(session_id).is_some() {
        info!(
            "🗑️ 移除 SESSION_CACHE 条目，防止任何残留消息: session_id={}",
            session_id
        );
        return 1;
    }

    0
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

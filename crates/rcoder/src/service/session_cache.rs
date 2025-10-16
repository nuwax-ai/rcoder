//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use dashmap::DashMap;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, RingBuffer};
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use std::sync::{LazyLock, RwLock, atomic::{AtomicBool, Ordering}};

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, SessionData>> = LazyLock::new(|| DashMap::new());

/// Project到当前活跃Session的映射 - 用于确保一个project_id只对应一个session_id
/// 当project_id对应的session_id发生变化时，会自动清理旧session的数据
pub static PROJECT_SESSION_MAP: LazyLock<DashMap<String, String>> = LazyLock::new(|| DashMap::new());

/// Session数据包装
pub struct SessionData {
    /// 循环消息缓存 - 固定大小1000条，使用ringbuf实现（不包含heartbeat）
    rb: tokio::sync::Mutex<HeapRb<UnifiedSessionMessage>>,
    /// 实时消息推送通道 - 使用异步有界channel
    tx: RwLock<Option<Sender<UnifiedSessionMessage>>>,
    /// 取消标志 - 取消后拒绝新消息
    is_cancelled: AtomicBool,
    /// 当前连接的取消令牌 - 用于实现单连接限制（新连接会取消旧连接）
    cancellation_token: RwLock<CancellationToken>,
}

impl SessionData {
    pub fn new(max_size: usize) -> Self {
        Self {
            rb: tokio::sync::Mutex::new(HeapRb::new(max_size)),
            tx: RwLock::new(None),
            is_cancelled: AtomicBool::new(false),
            cancellation_token: RwLock::new(CancellationToken::new()),
        }
    }

    /// 添加消息到循环缓存
    pub async fn add_message(&self, message: UnifiedSessionMessage) {
        let mut rb = self.rb.lock().await;
        // ringbuf 会自动循环覆盖，不需要手动检查大小
        let _ = rb.push_overwrite(message); // 如果缓冲区满，会覆盖最老的消息
    }

    /// 获取所有消息并清空缓存（用于SSE推送）
    pub async fn drain_messages(&self) -> Vec<UnifiedSessionMessage> {
        let mut rb = self.rb.lock().await;
        let mut messages = Vec::new();
        // 读取所有可用消息
        while let Some(message) = rb.try_pop() {
            messages.push(message);
        }
        messages
    }

    /// 从缓存中取出一条消息（用于循环发送）
    pub async fn pop_message(&self) -> Option<UnifiedSessionMessage> {
        let mut rb = self.rb.lock().await;
        rb.try_pop()
    }

    /// 获取消息数量
    pub async fn message_count(&self) -> usize {
        let rb = self.rb.lock().await;
        rb.occupied_len()
    }

    /// 清空所有消息（用于取消任务时清理）
    pub async fn clear_messages(&self) -> usize {
        let mut rb = self.rb.lock().await;
        let cleared_count = rb.occupied_len();
        rb.clear();
        cleared_count
    }

    /// 创建新连接，同时取消旧连接
    ///
    /// 返回 (sender, receiver, cancellation_token)
    /// - sender: 用于发送消息的 异步有界channel 发送端（该连接独享，用于心跳）
    /// - receiver: 用于接收消息的 channel 接收端
    /// - cancellation_token: 用于监听连接取消的令牌
    pub fn create_new_connection(&self, buffer_size: usize)
        -> (Sender<UnifiedSessionMessage>, Receiver<UnifiedSessionMessage>, CancellationToken)
    {
        // 1. 取消旧连接
        if let Ok(old_token) = self.cancellation_token.read() {
            old_token.cancel();
            info!("🔌 触发旧连接取消信号（CancellationToken）");
        }

        // 2. 创建新的取消令牌
        let new_token = CancellationToken::new();
        if let Ok(mut token_guard) = self.cancellation_token.write() {
            *token_guard = new_token.clone();
        }

        // 3. 创建新的异步有界消息 channel
        let (tx, rx) = mpsc::channel(buffer_size);
        if let Ok(mut channel_tx) = self.tx.write() {
            *channel_tx = Some(tx.clone());
        }

        // 4. 重置取消标志
        self.is_cancelled.store(false, Ordering::Release);

        info!("📡 创建新连接和取消令牌，使用异步有界channel，buffer_size={}", buffer_size);

        // 返回 tx 副本，让每个连接持有独立的 sender
        (tx, rx, new_token)
    }

    /// 设置取消状态
    pub fn set_cancelled(&self, cancelled: bool) {
        self.is_cancelled.store(cancelled, Ordering::Release);
        if cancelled {
            debug!("🚫 设置session为已取消状态");
        } else {
            debug!("✅ 重置session取消状态");
        }
    }

    /// 检查是否已取消
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Acquire)
    }

    /// 异步发送消息到channel（如果存在）
    pub async fn send_to_channel(&self, msg: UnifiedSessionMessage) -> bool {
        if let Ok(channel_tx) = self.tx.read() {
            if let Some(tx) = channel_tx.as_ref() {
                match tx.send(msg).await {
                    Ok(_) => {
                        debug!("📤 成功异步发送消息到 channel");
                        return true;
                    }
                    Err(_) => {
                        debug!("🔌 异步channel已关闭");
                    }
                }
            }
        }
        false
    }

    /// 彻底清空所有数据（用于取消任务）
    /// 清空 ringbuf + drop channel + 设置取消标志
    pub async fn clear_all(&self) -> usize {
        // 1. Drop channel（自动清空未发送消息）
        if let Ok(mut channel_tx) = self.tx.write() {
            *channel_tx = None;
            debug!("🗑️ 已drop channel");
        }

        // 2. 清空 ringbuf
        let mut rb = self.rb.lock().await;
        let cleared_count = rb.occupied_len();
        rb.clear();

        // 3. 设置取消标志
        self.is_cancelled.store(true, Ordering::Release);

        info!("🧹 彻底清空session数据: cleared_count={}", cleared_count);
        cleared_count
    }
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
pub async fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    let session_data = SESSION_CACHE
        .entry(session_id.to_string())
        .or_insert_with(|| SessionData::new(1000));

    // 检查是否已取消
    if session_data.is_cancelled() {
        debug!("🚫 Session已取消，丢弃消息: session_id={}", session_id);
        return Ok(());
    }

    let unified_message = notify.to_unified_message();

    // 添加调试日志
    debug!(
        "📥 推送消息到缓存: session_id={}, message_type={:?}, sub_type={}",
        session_id,
                                    unified_message.message_type,
        unified_message.sub_type
    );

    // 1. 存入 ringbuf（非 heartbeat 消息）
    if !matches!(unified_message.message_type, crate::model::SessionMessageType::Heartbeat) {
        session_data.add_message(unified_message.clone()).await;
        
        // 记录缓存中的消息数量
        let message_count = session_data.message_count().await;
        debug!(
            "📊 缓存消息数量: session_id={}, count={}",
            session_id,
            message_count
        );
    }

    Ok(())
}

/// 便捷函数：添加SessionNotify消息并管理Project-Session映射
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
    if let Some(session_data) = SESSION_CACHE.get(session_id) {
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
pub async fn clear_project_messages(project_id: &str, sessions_map: &dashmap::DashMap<String, crate::router::SessionInfo>) -> usize {
    let mut total_cleared = 0;

    // 遍历所有活跃的 session，找到属于指定 project_id 的 session
    for session_entry in sessions_map.iter() {
        let session_id = session_entry.key();
        let session_info = session_entry.value();

        // 检查这个 session 是否属于指定的 project_id
        if let Some(session_project_id) = &session_info.project_id {
            if session_project_id == project_id {
                // 清空这个 session 的消息
                let cleared_count = clear_session_messages(session_id).await;
                total_cleared += cleared_count;

                if cleared_count > 0 {
                    debug!(
                        "🧹 清空项目消息: project_id={}, session_id={}, cleared_count={}",
                        project_id, session_id, cleared_count
                    );
                }
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
    if let Some(entry) = PROJECT_SESSION_MAP.get(project_id) {
        let mapped_session_id = entry.value().clone(); // 克隆以避免借用问题

        // 如果session_id相同，不需要做任何操作
        if mapped_session_id == session_id {
            debug!(
                "📋 Project session映射未变化: project_id={}, session_id={}",
                project_id, session_id
            );
            return 0;
        }

        // session_id发生变化，需要清理旧session的数据
        info!(
            "🔄 检测到Project session变化: project_id={}, old_session_id={}, new_session_id={}",
            project_id, mapped_session_id, session_id
        );

        // 清理旧session的数据
        let cleared_count = clear_session_messages(&mapped_session_id).await;

        // 更新映射关系（entry会在作用域结束时自动释放）
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
        // 第一次建立映射关系
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

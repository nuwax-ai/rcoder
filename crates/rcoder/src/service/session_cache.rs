//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存统一会话消息到ringbuf循环缓冲区

use crate::{SessionNotify, UnifiedSessionMessage};
use anyhow::Result;
use dashmap::DashMap;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, RingBuffer};
use std::sync::LazyLock;

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, SessionData>> = LazyLock::new(|| DashMap::new());

/// Session数据包装
pub struct SessionData {
    /// 循环消息缓存 - 固定大小1000条，使用ringbuf实现
    rb: std::sync::Mutex<HeapRb<UnifiedSessionMessage>>,
}

impl SessionData {
    pub fn new(max_size: usize) -> Self {
        Self {
            rb: std::sync::Mutex::new(HeapRb::new(max_size)),
        }
    }

    /// 添加消息到循环缓存
    pub fn add_message(&self, message: UnifiedSessionMessage) {
        if let Ok(mut rb) = self.rb.lock() {
            // ringbuf 会自动循环覆盖，不需要手动检查大小
            let _ = rb.push_overwrite(message); // 如果缓冲区满，会覆盖最老的消息
        }
    }

    /// 获取所有消息并清空缓存（用于SSE推送）
    pub fn drain_messages(&self) -> Vec<UnifiedSessionMessage> {
        if let Ok(mut rb) = self.rb.lock() {
            let mut messages = Vec::new();
            // 读取所有可用消息
            while let Some(message) = rb.try_pop() {
                messages.push(message);
            }
            messages
        } else {
            Vec::new()
        }
    }

    /// 移除一条消息（用于SSE推送）
    pub fn pop_message(&self) -> Option<UnifiedSessionMessage> {
        if let Ok(mut rb) = self.rb.lock() {
            rb.try_pop()
        } else {
            None
        }
    }

    /// 获取消息数量
    pub fn message_count(&self) -> usize {
        if let Ok(rb) = self.rb.lock() {
            rb.occupied_len()
        } else {
            0
        }
    }
}

/// 便捷函数：添加SessionNotify消息（自动转换为统一格式）
pub fn push_session_update(session_id: &str, notify: SessionNotify) -> Result<()> {
    let unified_message = notify.to_unified_message();

    // 添加调试日志
    tracing::debug!(
        "📥 推送消息到缓存: session_id={}, message_type={:?}, sub_type={}",
        session_id,
        unified_message.message_type,
        unified_message.sub_type
    );

    let session_data = SESSION_CACHE
        .entry(session_id.to_string())
        .or_insert_with(|| SessionData::new(1000));

    session_data.add_message(unified_message);

    // 记录缓存中的消息数量
    let message_count = session_data.message_count();
    tracing::debug!(
        "📊 缓存消息数量: session_id={}, count={}",
        session_id,
        message_count
    );

    Ok(())
}

//! 全局Session缓存模块
//!
//! 使用LazyLock初始化全局DashMap，按session_id分组缓存SessionUpdate消息到循环数组

use std::sync::LazyLock;
use dashmap::DashMap;
use serde_json::Value;
use std::collections::VecDeque;

/// 全局Session缓存 - LazyLock初始化
pub static SESSION_CACHE: LazyLock<DashMap<String, SessionData>> = LazyLock::new(|| {
    DashMap::new()
});

/// Session数据包装
#[derive(Debug)]
pub struct SessionData {
    /// 循环消息缓存 - 固定大小1000条
    pub messages: std::sync::Mutex<VecDeque<Value>>,
    /// 最大缓存数量
    max_size: usize,
}

impl SessionData {
    pub fn new(max_size: usize) -> Self {
        Self {
            messages: std::sync::Mutex::new(VecDeque::with_capacity(max_size * 2)), // 预分配空间避免频繁扩容
            max_size,
        }
    }

    /// 添加消息到循环缓存
    pub fn add_message(&self, message: Value) {
        if let Ok(mut messages) = self.messages.lock() {
            // 如果满了，移除最老的消息
            if messages.len() >= self.max_size {
                messages.pop_front();
            }

            messages.push_back(message);
        }
    }

    /// 获取所有消息并清空缓存（用于SSE推送）
    pub fn drain_messages(&self) -> Vec<Value> {
        if let Ok(mut messages) = self.messages.lock() {
            let drained: Vec<Value> = messages.drain(..).collect();
            drained
        } else {
            Vec::new()
        }
    }

    /// 获取消息数量
    pub fn message_count(&self) -> usize {
        if let Ok(messages) = self.messages.lock() {
            messages.len()
        } else {
            0
        }
    }
}

/// 便捷函数：添加SessionUpdate消息
pub fn add_session_update(session_id: &str, session_update: Value) {
    let session_data = SESSION_CACHE
        .entry(session_id.to_string())
        .or_insert_with(|| SessionData::new(1000));

    session_data.add_message(session_update);
}

/// 便捷函数：获取并清空消息
pub fn drain_session_messages(session_id: &str) -> Vec<Value> {
    if let Some(session_data) = SESSION_CACHE.get(session_id) {
        session_data.drain_messages()
    } else {
        Vec::new()
    }
}

/// 便捷函数：获取session消息数量
pub fn get_session_message_count(session_id: &str) -> usize {
    if let Some(session_data) = SESSION_CACHE.get(session_id) {
        session_data.message_count()
    } else {
        0
    }
}
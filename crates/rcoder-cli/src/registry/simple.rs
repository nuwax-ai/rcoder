//! 简单会话注册表
//!
//! 基于 DashMap 的轻量级 SessionRegistry 实现，
//! 用于 CLI 单进程场景，替代 agent_runner 中的全局 AGENT_REGISTRY。

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use shared_types::{AgentStatus, ProjectAndAgentInfo, SessionEntry};

use agent_abstraction::SessionRegistry;

/// 简单会话注册表
///
/// 专为 CLI 单进程场景设计的 SessionRegistry 实现。
/// 使用 DashMap 存储，支持并发访问但预期仅有单个会话。
pub struct SimpleSessionRegistry {
    /// project_id → 会话条目
    entries: DashMap<String, ProjectAndAgentInfo>,
    /// session_id → project_id（反向索引）
    session_to_project: DashMap<String, String>,
}

impl SimpleSessionRegistry {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            session_to_project: DashMap::new(),
        }
    }
}

impl Default for SimpleSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry for SimpleSessionRegistry {
    type Entry = ProjectAndAgentInfo;

    fn get(&self, project_id: &str) -> Option<Self::Entry> {
        self.entries.get(project_id).map(|r| r.value().clone())
    }

    fn insert(&self, project_id: &str, session_id: &str, entry: Self::Entry) {
        self.session_to_project
            .insert(session_id.to_string(), project_id.to_string());
        self.entries.insert(project_id.to_string(), entry);
    }

    fn remove(&self, project_id: &str) -> Option<Self::Entry> {
        if let Some((_, entry)) = self.entries.remove(project_id) {
            let session_id = entry.session_id().to_string();
            self.session_to_project.remove(&session_id);
            Some(entry)
        } else {
            None
        }
    }

    fn contains(&self, project_id: &str) -> bool {
        self.entries.contains_key(project_id)
    }

    fn get_project_by_session(&self, session_id: &str) -> Option<String> {
        self.session_to_project
            .get(session_id)
            .map(|r| r.value().clone())
    }

    fn get_entry_by_session(&self, session_id: &str) -> Option<Self::Entry> {
        self.get_project_by_session(session_id)
            .and_then(|pid| self.get(&pid))
    }

    fn list_project_ids(&self) -> Vec<String> {
        self.entries.iter().map(|r| r.key().clone()).collect()
    }

    fn count(&self) -> usize {
        self.entries.len()
    }

    fn entry(&self, project_id: String) -> Entry<'_, String, Self::Entry> {
        self.entries.entry(project_id)
    }

    fn update_agent_status(&self, project_id: &str, status: AgentStatus) {
        if let Some(mut entry) = self.entries.get_mut(project_id) {
            entry.value_mut().status = status;
        }
    }

    fn update_last_activity(&self, project_id: &str, activity: DateTime<Utc>) {
        if let Some(mut entry) = self.entries.get_mut(project_id) {
            entry.value_mut().last_activity = activity;
        }
    }
}

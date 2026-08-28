//! 注册表移除操作（remove_by_* 三入口：按 project / 条件移除 / 按 session）。

use tracing::{debug, info};

use crate::service::agent_registry::{AgentSessionRegistry, ProjectAndAgentInfo};

impl AgentSessionRegistry {
    /// 通过 project_id 移除所有相关映射
    ///
    /// 返回被移除的 ProjectAndAgentInfo（如果存在）
    pub fn remove_by_project(&self, project_id: &str) -> Option<ProjectAndAgentInfo> {
        use dashmap::mapref::entry::Entry;

        info!(
            "[Registry] remove_by_project started: project_id={}",
            project_id
        );

        // 🎯 原子性地移除 project_to_session 并获取 session_id
        debug!("[Registry] Removing project_to_session mapping");
        let session_id = match self.project_to_session.entry(project_id.to_string()) {
            Entry::Occupied(entry) => {
                let (_, session_id) = entry.remove_entry(); // 原子性移除
                Some(session_id)
            }
            Entry::Vacant(_) => None,
        };
        debug!("[Registry] project_to_session mapping removed");

        // 移除反向映射
        if let Some(ref sid) = session_id {
            debug!("[Registry] Removing session_to_project mapping");
            self.session_to_project.remove(sid);
            debug!("[Registry] session_to_project mapping removed");
        }

        // 移除 agent_info
        debug!(
            "[Registry] Preparing to remove agent_info_map, project_id={}, current map length={}",
            project_id,
            self.agent_info_map.len()
        );

        // 检查 key 是否存在
        let key_exists = self.agent_info_map.contains_key(project_id);
        debug!(
            "[Registry] agent_info_map key existence check: project_id={}, exists={}",
            project_id, key_exists
        );

        // 执行 remove 操作
        debug!("[Registry] Executing agent_info_map.remove()...");
        let removed = self.agent_info_map.remove(project_id).map(|(_, v)| v);
        debug!(
            "[Registry] agent_info_map.remove() completed, removed={}, remaining_length={}",
            removed.is_some(),
            self.agent_info_map.len()
        );

        if removed.is_some() {
            info!(
                "[Registry] Removed Agent: project={}, session={:?}",
                project_id, session_id
            );
        }

        info!(
            "[Registry] remove_by_project completed: project_id={}",
            project_id
        );
        removed
    }

    /// 通过 project_id 移除映射（仅当 session_id 匹配时）
    ///
    /// 用于 spawned task 清理：当旧 session 的 spawned task 退出时调用，
    /// 避免误删已被新 session 替换的 registry 条目。
    ///
    /// ## 并发安全性
    ///
    /// 使用 DashMap entry API 实现原子性的"检查 session_id 并移除"，
    /// 避免 TOCTOU 竞态条件。
    pub fn remove_by_project_if_session_matches(
        &self,
        project_id: &str,
        expected_session_id: &str,
    ) -> Option<ProjectAndAgentInfo> {
        use dashmap::mapref::entry::Entry;

        match self.agent_info_map.entry(project_id.to_string()) {
            Entry::Occupied(entry) => {
                let current_session_id = entry.get().session_id.to_string();
                if current_session_id != expected_session_id {
                    info!(
                        "🔄 [Registry] Session mismatch, skip removal: project={}, expected={}, current={}",
                        project_id, expected_session_id, current_session_id
                    );
                    return None;
                }
                // session_id 匹配，安全移除
                let (_, removed_info) = entry.remove_entry();
                info!(
                    "🗑️ [Registry] Removing Agent (session matched): project={}, session={}",
                    project_id, expected_session_id
                );

                // 清理 project_to_session 和 session_to_project 映射
                // 使用 entry API 进行条件移除，避免 TOCTOU 竞态：
                // 在 agent_info_map 锁释放后，另一个线程可能已 register() 了新数据，
                // 直接 remove() 会误删新线程的映射
                if let Entry::Occupied(oe) = self.project_to_session.entry(project_id.to_string())
                    && oe.get() == expected_session_id
                {
                    oe.remove_entry();
                }
                if let Entry::Occupied(oe) = self
                    .session_to_project
                    .entry(expected_session_id.to_string())
                    && oe.get() == project_id
                {
                    oe.remove_entry();
                }

                Some(removed_info)
            }
            Entry::Vacant(_) => {
                debug!(
                    "🔄 [Registry] Agent already removed: project={}",
                    project_id
                );
                None
            }
        }
    }

    /// 通过 session_id 移除所有相关映射
    ///
    /// 返回被移除的 ProjectAndAgentInfo（如果存在）
    pub fn remove_by_session(&self, session_id: &str) -> Option<ProjectAndAgentInfo> {
        use dashmap::mapref::entry::Entry;

        // 🎯 原子性地移除 session_to_project 并获取 project_id
        let project_id = match self.session_to_project.entry(session_id.to_string()) {
            Entry::Occupied(entry) => {
                let (_, project_id) = entry.remove_entry(); // 原子性移除
                Some(project_id)
            }
            Entry::Vacant(_) => None,
        };

        // 如果找到 project_id，移除正向映射和 agent_info
        if let Some(ref pid) = project_id {
            self.project_to_session.remove(pid);
            let removed = self.agent_info_map.remove(pid).map(|(_, v)| v);

            if removed.is_some() {
                info!(
                    "🗑️ [Registry] Removing Agent via session: session={}, project={}",
                    session_id, pid
                );
            }

            return removed;
        }

        None
    }
}

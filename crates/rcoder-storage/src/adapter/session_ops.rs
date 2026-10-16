//! Session 映射操作（从 adapter.rs 拆出，extension-impl）
//!
//! session_index 的读写面：session↔project 映射登记/清理/反查。
//! project/session 的 CRUD 主流程仍在 adapter.rs。

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use shared_types::ProjectAndContainerInfo;

use super::{ProjectAdapter, container_entry_key};
use tracing::debug;

/// Structural revisions are metadata cursors, not physical runner identities.
pub(crate) fn session_container_matches(
    expected: &ProjectAndContainerInfo,
    current: &ProjectAndContainerInfo,
) -> bool {
    let (Some(expected_basic), Some(current_basic)) =
        (expected.container_info(), current.container_info())
    else {
        return false;
    };
    expected.project_id() == current.project_id()
        && expected.persistence_identity().generation == current.persistence_identity().generation
        && expected
            .service_type()
            .zip(current.service_type())
            .is_some_and(|(expected, current)| expected.same_container_family(current))
        && expected.user_id() == current.user_id()
        && expected.pod_id() == current.pod_id()
        && expected
            .persistence_identity()
            .container
            .as_ref()
            .map(|c| &c.generation)
            == current
                .persistence_identity()
                .container
                .as_ref()
                .map(|c| &c.generation)
        && !expected_basic.container_id.is_empty()
        && expected_basic.container_id == current_basic.container_id
        && expected_basic.container_name == current_basic.container_name
        && expected_basic.project_id == current_basic.project_id
        && expected_basic.workload_uid == current_basic.workload_uid
        && expected
            .persistence_identity()
            .container
            .as_ref()
            .is_none_or(|identity| {
                identity.physical_uid.as_deref() == Some(expected_basic.container_id.as_str())
            })
        && current
            .persistence_identity()
            .container
            .as_ref()
            .is_none_or(|identity| {
                identity.physical_uid.as_deref() == Some(current_basic.container_id.as_str())
            })
}

impl ProjectAdapter {
    // ========== Session 操作 ==========

    /// Append to the exact runner captured before dispatch. Preserve all current
    /// project metadata and other sessions, including concurrent request updates.
    pub(crate) fn append_session_for_container(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(!session_id.is_empty(), "Session identity is empty");
        let project_id = expected.project_id();
        let _project_guard = self.project_locks.entry_by_ref(project_id);
        let Some(current) = self.get(project_id) else {
            return Ok(false);
        };
        let key = container_entry_key(&current);
        let _container_guard = self.container_locks.entry_by_ref(&key);
        if !session_container_matches(expected, &current)
            || current
                .persistence_identity()
                .retired_sessions
                .get(session_id)
                != expected
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
        {
            return Ok(false);
        }
        anyhow::ensure!(
            self.session_index
                .view(session_id, |_, owner| owner.1 == project_id)
                .unwrap_or(true),
            "Session belongs to another project"
        );
        Ok(self.add_session_to_project(project_id, session_id))
    }

    /// 通过 session_id 获取项目信息
    ///
    /// 读时清理孤儿条目：如果 session_index 指向的 project 不存在，自动清理。
    /// 同时验证 session 是否仍在 project 的 sessions 集合中，防止已清除的 session 被误访问。
    pub fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let pid = self.session_index.view(session_id, |_, v| v.1.clone())?;

        match self.projects.view(&pid, |_, v| v.clone()) {
            Some(info) => {
                // 验证 session 是否仍在 sessions 集合中
                if info.sessions().contains(session_id) {
                    Some(info)
                } else {
                    // session 已被清除，清理索引
                    debug!(
                        "[STORAGE] session already removed from project, cleaning index: session_id={}, project_id={}",
                        session_id, pid
                    );
                    self.session_index.remove(session_id);
                    None
                }
            }
            None => {
                // 孤儿条目：session_index 指向的 project 已不存在，清理
                debug!(
                    "[STORAGE] orphan session_index entry detected, cleaning: session_id={}, project_id={}",
                    session_id, pid
                );
                self.session_index.remove(session_id);
                None
            }
        }
    }

    /// 向已有 project 追加 session（C1 修复推荐路径）
    ///
    /// 用于 chat 响应后只追加 session、不重建整个 ProjectAndContainerInfo 的场景。
    /// 多 session 并存语义：一个 project 可以有多个并发活跃 session。
    ///
    /// **非原子说明**：本方法跨 3 个 DashMap（projects + session_index + project_to_container），
    /// 不是单步原子。设计上：
    /// - project 不存在时**完全不写** session_index（避免孤儿条目）
    /// - project 存在时先 entry 写 projects.sessions，再写 session_index
    /// - 并发 remove(project) 的竞态由 `get_by_session_id` 读时清理兜底
    ///
    /// 返回 false 表示 project 不存在（调用方需要走 insert_with_session 完整路径）。
    pub fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        // entry API: 先检查 project 是否存在，存在时原子地把 sid 加入 sessions 集合
        let mut existed = false;
        let mut ck_opt: Option<String> = None;
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.add_session(session_id);
            existed = true;
            // 顺便拿容器键（container_name 或 logical_id 回退），避免后续再锁一次 project_to_container
            ck_opt = Some(container_entry_key(info));
        }

        if !existed {
            // project 不存在：不写 session_index，避免孤儿条目（修复 Bug 1）
            return false;
        }

        // 维护 session_index（只在 project 存在时写）
        // 兜底：若键为空（边界场景），从 project_to_container 取
        let ck = ck_opt.unwrap_or_else(|| {
            self.project_to_container
                .view(project_id, |_, v| v.clone())
                .unwrap_or_default()
        });
        self.session_index
            .insert(session_id.to_string(), (ck.clone(), project_id.to_string()));

        // 同步更新容器活跃时间（仅当 ck 非空）
        if !ck.is_empty() {
            self.containers.view(&ck, |_, ce| ce.update_activity());
        }

        true
    }

    /// Append routing to the exact captured builder generation without replacing
    /// its metadata or physical container. All project mutation occurs in one entry.
    pub(crate) fn add_userapp_session_if_generation(
        &self,
        app_id: &str,
        expected_generation: &str,
        session_id: &str,
        session_generation: &str,
    ) -> anyhow::Result<bool> {
        let key = match self.projects.entry(app_id.to_owned()) {
            Entry::Occupied(mut entry) => {
                let current = entry.get();
                if current.persistence_identity().generation != expected_generation
                    || current.service_type() != Some(shared_types::ServiceType::UserappBuilder)
                {
                    return Ok(false);
                }
                anyhow::ensure!(
                    current
                        .persistence_identity()
                        .sessions
                        .get(session_id)
                        .is_none_or(|generation| generation == session_generation),
                    "Physical UserApp SID already has another generation"
                );
                let info = Arc::make_mut(entry.get_mut());
                info.restore_session_identity(session_id, session_generation.to_owned());
                info.add_session(session_id);
                container_entry_key(info)
            }
            Entry::Vacant(_) => return Ok(false),
        };
        // The project guard has ended before touching the reverse/container maps.
        self.session_index
            .insert(session_id.to_owned(), (key.clone(), app_id.to_owned()));
        self.containers
            .view(&key, |_, entry| entry.update_activity());
        Ok(true)
    }

    /// 恢复专用变体（boot 加载 / 回源 hydrate）：session 集合与索引登记语义
    /// 与 [`Self::add_session_to_project`] 一致，但不刷新 `last_activity` 与
    /// 容器活跃——恢复动作不代表用户活跃，时间戳以持久化行为准。
    pub fn restore_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        let mut existed = false;
        let mut ck_opt: Option<String> = None;
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.restore_session(session_id);
            existed = true;
            ck_opt = Some(container_entry_key(info));
        }
        if !existed {
            return false;
        }
        let ck = ck_opt.unwrap_or_else(|| {
            self.project_to_container
                .view(project_id, |_, v| v.clone())
                .unwrap_or_default()
        });
        self.session_index
            .insert(session_id.to_string(), (ck, project_id.to_string()));
        true
    }

    /// 插入项目并添加 session 映射（原子操作，消除 CAS 竞态）
    ///
    /// **多 session 语义（C2 修复）**：本方法只往 session_index *追加* 条目，
    /// 不再清除该 project 之前关联的其他 session。一个 project 可以有多个并发活跃 session。
    ///
    /// # Errors
    /// 如果 `service_type` 未设置，透传 `insert` 的错误。
    pub fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // 执行 insert（维护容器引用计数）
        self.insert(project_id.clone(), info)?;

        // 追加 session 索引（不清除旧 session）
        if let Some(sid) = session_id {
            let ck = self
                .project_to_container
                .view(&project_id, |_, v| v.clone())
                .unwrap_or_default();
            self.session_index
                .insert(sid.to_string(), (ck, project_id.clone()));

            // entry API: 精确锁定单条记录，把 sid 加入 sessions 集合
            if let Entry::Occupied(mut e) = self.projects.entry(project_id) {
                let info = Arc::make_mut(e.get_mut());
                info.add_session(sid);
            }
        }
        Ok(())
    }

    /// 清空该 project 的所有 session（agent stop 场景）
    ///
    /// 语义：当用户主动 stop agent 时，所有 session 失效。
    /// 若只想清单个 session（如 SSE 流正常结束），请用 `clear_session_one`。
    ///
    /// 顺序：先清理 session_index，再清理 projects，避免并发访问时出现不一致。
    pub fn clear_session(&self, project_id: &str) {
        // 先获取所有 session_id（用于后续清理 session_index）
        let cleared_sids: Vec<String> = self
            .projects
            .view(project_id, |_, v| v.sessions().into_iter().collect())
            .unwrap_or_default();

        // 先清理 session_index，防止并发访问时 get_by_session_id 返回已清除的 session
        for sid in &cleared_sids {
            self.session_index.remove(sid);
        }

        // 再清理 projects 中的 sessions 集合
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.clear_all_sessions();
        }

        if !cleared_sids.is_empty() {
            debug!(
                "[STORAGE] cleared all sessions: project_id={}, count={}",
                project_id,
                cleared_sids.len()
            );
        }
    }

    /// 清除单个 session（保留 project 的其他 session）
    ///
    /// 用于 SSE 流自然结束、单 session 取消等场景。
    /// 返回 true 表示该 session 之前存在。
    ///
    /// 顺序：先清理 session_index，再清理 projects，避免并发访问时出现不一致。
    pub fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        // 先清理 session_index，防止并发访问时 get_by_session_id 返回已清除的 session
        let was_in_index = self.session_index.remove(session_id).is_some();

        // 再清理 projects 中的 sessions 集合
        let mut removed_from_project = false;
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            removed_from_project = info.remove_session(session_id);
        }

        let removed = was_in_index || removed_from_project;
        if removed {
            debug!(
                "[STORAGE] cleared one session: project_id={}, sid={}, from_index={}, from_project={}",
                project_id, session_id, was_in_index, removed_from_project
            );
        }
        removed
    }

    /// A delayed logical cleanup cannot clear a later physical SID generation.
    pub(crate) fn clear_session_one_if_generation(
        &self,
        project_id: &str,
        session_id: &str,
        generation: &str,
    ) -> bool {
        let _project_guard = self.project_locks.entry_by_ref(project_id);
        let removed = match self.projects.entry(project_id.to_owned()) {
            Entry::Occupied(mut entry)
                if entry
                    .get()
                    .persistence_identity()
                    .sessions
                    .get(session_id)
                    .is_some_and(|current| current == generation) =>
            {
                Arc::make_mut(entry.get_mut()).remove_session(session_id)
            }
            _ => false,
        };
        if removed {
            self.session_index
                .remove_if(session_id, |_, (_, owner)| owner == project_id);
        }
        removed
    }

    // ========== Session → Container ==========

    /// 通过 session_id 获取容器名称（view: 两次锁获取均立即释放）
    pub fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        let ck = self.session_index.view(session_id, |_, v| v.0.clone())?;
        self.containers
            .view(&ck, |_, ce| ce.info().container_name.clone())
    }
}

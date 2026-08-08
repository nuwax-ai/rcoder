//! 项目适配器：纯 DashMap 内存存储 + RAII 自动资源回收
//!
//! 替代 DuckDB 内存模式：
//! - 热路径 O(1) 无锁读（DashMap 分片）
//! - 引用计数容器管理（共享容器安全）
//! - RAII 清理（移除 project 时自动销毁无引用的容器）
//!
//! ## DashMap 使用规范
//!
//! - **读操作**: 全部使用 `view()`（闭包结束立即释放读锁）
//! - **写操作**: 使用 `entry()` API（精确锁定单条记录）
//! - **禁止**: `get()` 返回的 guard 跨 map 操作（死锁风险）
//!
//! 容器记录类方法（save/delete/查询/闲置扫描/引用计数）见 `adapter_container_ops.rs`
//! （同类型 extension-impl 拆分）。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::DashSet;
use dashmap::mapref::entry::Entry;
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use tracing::{debug, info};

use super::resource_reaper::CleanupRequest;
use super::types::StorageStats;
use shared_types::ContainerEntry;

/// 项目适配器
///
/// 纯内存存储，使用 DashMap 分片实现高并发。
/// 容器引用计数归零时通过 channel 触发异步物理销毁（RAII）。
///
/// ## 反向索引
///
/// 为避免全量遍历，维护 3 个反向索引：
/// - `container_id_to_key`: container_id → container_key（O(1) 按容器 ID 查找）
/// - `user_id_to_project_ids`: user_id → project_id 集合（多值，按用户 ID 查找其全部 project）
/// - `pod_id_to_project_id`: pod_id → project_id（O(1) 按 pod ID 查找）
///
/// 索引在 `insert`、`remove`、`save_container`、`delete_container_with_projects` 中同步维护。
#[derive(Clone)]
pub struct ProjectAdapter {
    /// project_id → project 信息（主存储）
    ///
    /// 字段 `pub(super)`：供 `adapter_container_ops.rs`（同类型 extension-impl 拆分）访问。
    pub(super) projects: DashMap<String, Arc<ProjectAndContainerInfo>>,
    /// container_key → 容器条目（带引用计数，Arc 共享确保原子状态一致）
    pub(super) containers: DashMap<String, Arc<ContainerEntry>>,
    /// session_id → (container_key, project_id)
    session_index: DashMap<String, (String, String)>,
    /// project_id → container_key（反向索引）
    pub(super) project_to_container: DashMap<String, String>,
    /// RAII 清理通道（bounded，try_send 非阻塞，满时丢弃并告警）
    pub(super) cleanup_tx: tokio::sync::mpsc::Sender<CleanupRequest>,
    /// K8s namespace（用于构建 K8s Service FQDN）
    pub(super) namespace: String,
    /// K8s 集群域名
    pub(super) cluster_domain: String,

    // === 反向索引（CQRS：写入时维护，读取时 O(1)） ===
    /// container_id → container_key（按容器 ID 快速查找）
    pub(super) container_id_to_key: DashMap<String, String>,
    /// user_id → project_id 集合（多值索引）
    ///
    /// user_id 是 1:N（一个 user 可关联多个 project），用 DashSet 存全部 project_id。
    /// 服务于：
    /// - `find_projects_by_user_id`（cleanup 判断容器能否销毁需枚举该 user 的全部 project）
    /// - `find_by_user_id` / `get_container_by_user_id`（取任一即可，同 user 的同 ServiceType
    ///   项目共享同一容器）
    pub(super) user_id_to_project_ids: DashMap<String, DashSet<String>>,
    /// pod_id → project_id（按 pod ID 快速查找）
    pub(super) pod_id_to_project_id: DashMap<String, String>,
}

impl ProjectAdapter {
    /// 创建新的项目适配器
    ///
    /// 返回 (adapter, cleanup_receiver)。
    /// cleanup_receiver 需要传给 ResourceReaper 以处理容器销毁。
    pub fn new(
        namespace: String,
        cluster_domain: String,
    ) -> (Self, tokio::sync::mpsc::Receiver<CleanupRequest>) {
        let (tx, rx) =
            tokio::sync::mpsc::channel(crate::storage::resource_reaper::CLEANUP_CHANNEL_CAPACITY);
        let adapter = Self {
            projects: DashMap::new(),
            namespace,
            cluster_domain,
            containers: DashMap::new(),
            session_index: DashMap::new(),
            project_to_container: DashMap::new(),
            cleanup_tx: tx,
            container_id_to_key: DashMap::new(),
            user_id_to_project_ids: DashMap::new(),
            pod_id_to_project_id: DashMap::new(),
        };
        info!("[STORAGE] ProjectAdapter initialized (DashMap, RAII enabled)");
        (adapter, rx)
    }

    // ========== Project CRUD ==========

    /// 获取项目信息（view: 闭包结束读锁立即释放）
    pub fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.view(project_id, |_, v| v.clone())
    }

    /// 插入或更新项目信息
    ///
    /// 自动维护容器引用计数。
    /// 如果 project 已存在且容器变更，旧容器引用 -1。
    ///
    /// # Errors
    /// 如果 `service_type` 未设置，返回错误（Fail Fast）。
    pub fn insert(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<()> {
        // DashMap 键：优先 container_name（跨重建稳定、含 service_type 前缀防跨类型碰撞），
        // 无容器信息时回退裸 logical_id（仅占位，不建容器条目）。
        let key = container_entry_key(&info);

        // 读取旧键（view: 读后立即释放锁）
        let old_ck = self
            .projects
            .view(&project_id, |_, v| container_entry_key(v));

        // 容器是否变更
        let container_changed = match &old_ck {
            Some(old) => *old != key,
            None => true, // 新 project，需要 inc_ref
        };

        // 旧容器引用 -1（容器变更时）
        if let Some(old) = old_ck
            && container_changed
        {
            self.dec_container_ref(&old);
        }

        // 取出 temp Arc（set_container 时建的临时条目）—— clone 出来释放 info 的借用
        let temp_entry = info.container().cloned();
        let mut info = info; // 取得所有权，便于后续 Arc::make_mut 回写共享 Arc

        // 有容器信息时：Arc 共享逻辑——让 projects[pid] 与 containers[name] 共享同一 Arc
        if let Some(temp_entry) = temp_entry {
            let st = match info.service_type() {
                Some(st) => st,
                None => {
                    tracing::error!(
                        "[STORAGE] service_type is None, cannot insert project: project_id={}, key={}",
                        project_id,
                        key
                    );
                    return Err(anyhow::anyhow!(
                        "service_type is required for project insert: project_id={}",
                        project_id
                    ));
                }
            };
            if container_changed {
                // 键变了：把 temp Arc 共享到 containers[key]
                let need_repoint = match self.containers.entry(key.clone()) {
                    Entry::Occupied(e) => {
                        let ce = e.get();
                        ce.inc_ref(); // 另一 project 已有此容器（含 refcount=0 的游离容器），引用 +1
                        ce.update_activity(); // 复活:刷新 last_activity,避免刚回收池中的容器被立刻判 idle
                        Some(Arc::clone(ce)) // 回指权威条目
                    }
                    Entry::Vacant(e) => {
                        e.insert(Arc::clone(&temp_entry)); // 共享 temp Arc
                        None // info 已持 temp，无需回指
                    }
                };
                if let Some(existing_arc) = need_repoint {
                    Arc::make_mut(&mut info).set_container_arc(Some(existing_arc));
                }
            } else {
                // 键不变（容器重建）：原地刷新 containers[key]（保持 Arc 身份不变），
                // info 回指同一权威 Arc——后续 containers[name] 的刷新 info 自动可见。
                if let Some(existing_ref) = self.containers.get(&key) {
                    let existing_arc = Arc::clone(&*existing_ref);
                    let old_cid = existing_ref.info().container_id;
                    let new_cid = temp_entry.info().container_id;
                    if old_cid != new_cid {
                        existing_ref.update(temp_entry.info(), st);
                    }
                    drop(existing_ref); // 释放读锁后再操作其他 map
                    if old_cid != new_cid {
                        self.container_id_to_key.remove(&old_cid);
                    }
                    Arc::make_mut(&mut info).set_container_arc(Some(existing_arc));
                }
            }
        }

        // 写入主存储和索引
        self.project_to_container
            .insert(project_id.clone(), key.clone());
        self.projects.insert(project_id.clone(), info.clone());

        // 维护反向索引：container_id → 容器键
        if let Some(c) = info.container_info() {
            self.container_id_to_key.insert(c.container_id.clone(), key);
        }
        // user_id → project_id 集合（多值）：entry API 原子插入
        if let Some(uid) = info.user_id() {
            self.user_id_to_project_ids
                .entry(uid.to_string())
                .or_default()
                .insert(project_id.clone());
        }
        // pod_id → project_id
        if let Some(pid) = info.pod_id() {
            self.pod_id_to_project_id
                .insert(pid.to_string(), project_id);
        }
        Ok(())
    }

    /// 删除项目（RAII 核心）
    ///
    /// 自动清理 session 索引和容器引用计数。
    /// 容器引用归零时**保留容器条目**(刷新活跃时间,交 cleaner idle 回收),
    /// 不再立即触发物理销毁 —— 避免短间隔 chat 反复重建容器导致 transport error。
    pub fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 1. 先从主存储移除，获取 info 所有权（避免后续从 map 读取时被并发修改）
        let (_, info) = self.projects.remove(project_id)?;

        // 2. 从已获取的 info 中读取所有 session_id 并清理 session_index（C2 多 session 适配）
        for sid in info.sessions() {
            self.session_index.remove(&sid);
        }

        // 2.1 清理反向索引
        // user_id：entry API 在单个 OccupiedEntry guard 内完成「从 DashSet 移除 project_id
        // + 判空 + 摘除整个 user_id 条目」，原子且无并发 insert 竞态。
        if let Some(uid) = info.user_id()
            && let Entry::Occupied(e) = self.user_id_to_project_ids.entry(uid.to_string())
        {
            e.get().remove(project_id); // DashSet::remove 是 &self
            if e.get().is_empty() {
                e.remove_entry(); // 集合空才摘除 user_id → DashSet 条目
            }
        }
        if let Some(pid) = info.pod_id() {
            self.pod_id_to_project_id.remove(pid);
        }

        // 3. 清理 container 映射（防御性：即使映射缺失也返回已获取的 info）
        let container_key = self
            .project_to_container
            .remove(project_id)
            .map(|(_, ck)| ck);

        // 4. 容器引用计数减 1，归零时触发 RAII 清理
        if let Some(ck) = container_key {
            self.dec_container_ref(&ck);
        } else {
            debug!(
                "[STORAGE] WARNING: project_to_container missing for {}, skipping dec_ref",
                project_id
            );
        }

        debug!(
            "[STORAGE] removed project: {} (cleared {} sessions)",
            project_id,
            info.session_count()
        );
        Some(info)
    }

    /// 检查项目是否存在
    pub fn contains_key(&self, project_id: &str) -> bool {
        self.projects.contains_key(project_id)
    }

    /// 获取所有项目
    pub fn iter(&self) -> Vec<(String, Arc<ProjectAndContainerInfo>)> {
        self.projects
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    /// 项目数量
    pub fn len(&self) -> usize {
        self.projects.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    // ========== Session 操作 ==========

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

    /// 单值更新 session（已废弃，新代码请用 `insert_with_session` 或新 `add_session` 路径）
    ///
    /// 历史问题：本方法非原子（write_session_index + entry 两步），并发调用会产生 session 互踩。
    /// 多 session 模型下，请改用 `insert_with_session` 走 add-only 路径。
    #[deprecated(
        since = "0.0.0",
        note = "非原子且语义已变更，请用 `insert_with_session` 走多 session 路径"
    )]
    pub fn update_session(&self, project_id: &str, session_id: &str) {
        // 走 add_session 语义，不再覆盖（兼容旧调用点 + 多 session）
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.add_session(session_id);
        }

        // 维护 session_index（追加，不清旧）
        let ck = self
            .project_to_container
            .view(project_id, |_, v| v.clone())
            .unwrap_or_default();
        self.session_index
            .insert(session_id.to_string(), (ck, project_id.to_string()));

        // 更新容器活跃时间
        if let Some(ck) = self.project_to_container.view(project_id, |_, v| v.clone()) {
            self.containers.view(&ck, |_, ce| ce.update_activity());
        }
    }

    /// 原子更新 session（已废弃，CAS 语义在多 session 模型下不再适用）
    #[deprecated(since = "0.0.0", note = "CAS 语义在多 session 模型下不再适用")]
    #[allow(dead_code)]
    pub fn update_session_atomic(
        &self,
        project_id: &str,
        new_session_id: &str,
        _expected_current_session_id: Option<&str>,
    ) -> bool {
        // 简化：直接走 add_session（不再做 CAS 检查）
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.add_session(new_session_id);
        }
        let ck = self
            .project_to_container
            .view(project_id, |_, v| v.clone())
            .unwrap_or_default();
        self.session_index
            .insert(new_session_id.to_string(), (ck, project_id.to_string()));
        true
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

    // ========== Session → Container ==========

    /// 通过 session_id 获取容器名称（view: 两次锁获取均立即释放）
    pub fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        let ck = self.session_index.view(session_id, |_, v| v.0.clone())?;
        self.containers
            .view(&ck, |_, ce| ce.info().container_name.clone())
    }

    // ========== 活动时间更新 ==========

    /// 更新项目活动时间，返回更新后的时间戳
    pub fn update_activity(&self, project_id: &str) -> Option<DateTime<Utc>> {
        let mut result = None;
        let mut container_key = None;

        // entry API: 精确锁定单条记录，修改 + 读取在同一写锁内
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.update_activity();
            result = Some(info.last_activity());
            container_key = Some(container_entry_key(info));
        }

        // entry 已释放，安全访问 containers map
        if let Some(ck) = container_key {
            self.containers.view(&ck, |_, ce| ce.update_activity());
        }

        result
    }

    /// 更新 session 活动时间
    pub fn update_session_activity(&self, session_id: &str) -> bool {
        let Some((ck, pid)) = self.session_index.view(session_id, |_, v| v.clone()) else {
            return false;
        };

        // entry API: 精确更新 project 活动时间
        if let Entry::Occupied(mut e) = self.projects.entry(pid) {
            let info = Arc::make_mut(e.get_mut());
            info.update_activity();
        }

        // view: 原子操作更新容器活跃时间
        self.containers.view(&ck, |_, ce| ce.update_activity());
        true
    }

    // ========== Agent 状态更新 ==========

    /// 更新 Agent 状态
    pub fn update_agent_status(
        &self,
        project_id: &str,
        status_code: i32,
        status_name: &str,
    ) -> bool {
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.set_status(Some(code_to_agent_status(status_code, status_name)));
            true
        } else {
            false
        }
    }

    // ========== 清理相关 ==========

    /// 获取存储统计信息
    pub fn get_stats(&self) -> StorageStats {
        let total_containers = self.containers.len();
        let total_projects = self.projects.len();
        let active_sessions = self.session_index.len();
        let mut projects_by_service_type = std::collections::HashMap::new();

        for entry in self.containers.iter() {
            let st = entry.value().service_type();
            *projects_by_service_type.entry(st).or_insert(0usize) += 1;
        }

        StorageStats {
            total_containers,
            total_projects,
            active_sessions,
            projects_by_service_type,
        }
    }

    // ========== 调试方法 ==========

    /// 获取存储摘要（替代 SQL raw query）
    pub fn dump_summary(&self) -> String {
        format!(
            "projects={}, containers={}, sessions={}",
            self.projects.len(),
            self.containers.len(),
            self.session_index.len()
        )
    }

    // ========== 内部方法 ==========

    // 注：原 write_session_index helper 已移除。
    // 多 session 模型下 insert_with_session 直接追加 session_index 条目，
    // 不再需要"读旧 → 清旧 → 写新"的两步操作。
}

impl std::fmt::Debug for ProjectAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectAdapter")
            .field("projects", &self.projects.len())
            .field("containers", &self.containers.len())
            .field("sessions", &self.session_index.len())
            .field("idx_container_id", &self.container_id_to_key.len())
            .field("idx_user_id", &self.user_id_to_project_ids.len())
            .field("idx_pod_id", &self.pod_id_to_project_id.len())
            .finish()
    }
}

/// Agent 状态码 → AgentStatus 转换
fn code_to_agent_status(code: i32, _name: &str) -> shared_types::AgentStatus {
    match code {
        0 => shared_types::AgentStatus::Idle,
        1 => shared_types::AgentStatus::Active,
        2 => shared_types::AgentStatus::Terminating,
        3 => shared_types::AgentStatus::Pending,
        _ => shared_types::AgentStatus::Idle,
    }
}

/// 计算 `containers` DashMap 的键：有容器信息用 `container_name`（真实容器名、跨重建稳定、
/// 含 service_type 前缀 `computer-agent-runner-`/`web-agent-runner-` 天然防跨类型碰撞）；
/// 无容器信息时回退裸 `logical_id`（仅占位，不会建容器条目，且 container_name 必含 prefix
/// 不会与裸 id 相撞）。
///
/// 注意：此键仅用于 DashMap 分组/refcount，**不等于** `ProjectAndContainerInfo::container_key()`
/// （后者返回裸 logical id，供 RAII 清理 identifier 与外部消费者用）。
fn container_entry_key(info: &ProjectAndContainerInfo) -> String {
    match info.container_info() {
        Some(c) => c.container_name.clone(),
        None => info.container_key().to_string(),
    }
}

// ========== pingora backend 地址解析 ==========

impl ProjectAdapter {
    /// 解析 pingora 反向代理的 backend 地址。
    ///
    /// - K8s:headless Service FQDN(`{container_name}-svc.{ns}.svc.{domain}`),经 K8s DNS
    ///   解析;Pod 重建后 Service selector 选到新 Pod,DNS 自动指向新 IP,客户端重连即
    ///   恢复,无需 rcoder 重注册/重查(与 `register_vnc_backend` 的 vnc_backends 对齐)。
    /// - Docker:容器 IP(直连)。
    fn resolve_backend_addr(&self, info: &ContainerBasicInfo) -> String {
        if shared_types::is_kubernetes_runtime() {
            shared_types::build_k8s_service_fqdn(
                &info.container_name,
                &self.namespace,
                &self.cluster_domain,
            )
        } else {
            info.container_ip.clone()
        }
    }
}

// ========== ContainerLookup trait 实现 ==========

impl shared_types::ContainerLookup for ProjectAdapter {
    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    ///
    /// user_id 是 1:N（一个 user 可有多个 project），无法用单值索引精确反查。
    /// 此处全量扫描（`find_projects_by_user_id`，已按 `service_type` 过滤）取任一
    /// 匹配 project，再经 `container_info_by_project` 走 `containers[name]` 权威源取 IP——
    /// 同 user 的 Computer 项目共享同一容器，任取一个即可。O(N)，N 为该 user 的 project 数。
    fn find_by_user_id(&self, user_id: &str, service_type: &ServiceType) -> Option<String> {
        // 委托 get_container_by_user_id（同一查找逻辑：扫描 + containers[name] 权威源），
        // 仅取 container_ip。同 user 的 Computer 项目共享同一容器，任取一个即可。
        self.get_container_by_user_id(user_id, service_type)
            .map(|c| self.resolve_backend_addr(&c))
    }

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    ///
    /// 通过 project_to_container 索引找到 container_key，
    /// 然后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_project_id(&self, project_id: &str, service_type: &ServiceType) -> Option<String> {
        // clone 出 container_key 后立即释放 project_to_container 读锁
        let container_key = self.project_to_container.get(project_id)?.value().clone();
        let entry = self.containers.get(&container_key)?;
        // 校验 service_type，防止串用
        if entry.service_type() != *service_type {
            debug!(
                "[CONTAINER_LOOKUP] service_type mismatch: expected={:?}, found={:?}, project_id={}",
                service_type,
                entry.service_type(),
                project_id
            );
            return None;
        }
        Some(self.resolve_backend_addr(&entry.info()))
    }

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    ///
    /// 通过 pod_id_to_project_id 索引找到 project_id，
    /// 然后通过 project_to_container 索引找到 container_key，
    /// 最后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None，
    /// 避免同一 pod_id 下跨 ServiceType 容器互相串用。
    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String> {
        // 索引链查找：每步 clone 出 key 后立即释放读锁，避免跨 map 同时持锁
        let project_id = self.pod_id_to_project_id.get(pod_id)?.value().clone();
        let container_key = self.project_to_container.get(&project_id)?.value().clone();
        let entry = self.containers.get(&container_key)?;
        // 校验 service_type，防止串用
        if entry.service_type() != *service_type {
            debug!(
                "[CONTAINER_LOOKUP] service_type mismatch: expected={:?}, found={:?}, pod_id={}",
                service_type,
                entry.service_type(),
                pod_id
            );
            return None;
        }
        Some(self.resolve_backend_addr(&entry.info()))
    }

    /// 按 project_id 反查项目归属 scope（tenant_id/space_id/isolation_type）。
    ///
    /// 直接查 `projects` map（O(1)），不走 container 索引链。命中项目的 service_type
    /// 必须与入参一致（防串用，与 find_by_project_id 同策略）。供 Pingora 注入
    /// `X-Ttyd-Tenant-Id`/`X-Ttyd-Space-Id`，agent_runner 据此解析终端 cwd。
    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<shared_types::ProjectScope> {
        let info = self.projects.get(project_id)?;
        // 校验 service_type，防止跨 ServiceType 串用
        if info.service_type().as_ref() != Some(service_type) {
            debug!(
                "[CONTAINER_LOOKUP] find_project_scope service_type mismatch: expected={:?}, found={:?}, project_id={}",
                service_type,
                info.service_type(),
                project_id
            );
            return None;
        }
        Some(shared_types::ProjectScope {
            tenant_id: info.tenant_id().map(str::to_string),
            space_id: info.space_id().map(str::to_string),
            isolation_type: info.isolation_type().map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests;

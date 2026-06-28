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

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::DashSet;
use dashmap::mapref::entry::Entry;
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use tracing::{debug, info};

use super::container_entry::ContainerEntry;
use super::resource_reaper::CleanupRequest;
use super::types::{IdleContainerInfo, StorageStats};

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
    projects: DashMap<String, Arc<ProjectAndContainerInfo>>,
    /// container_key → 容器条目（带引用计数，Arc 共享确保原子状态一致）
    containers: DashMap<String, Arc<ContainerEntry>>,
    /// session_id → (container_key, project_id)
    session_index: DashMap<String, (String, String)>,
    /// project_id → container_key（反向索引）
    project_to_container: DashMap<String, String>,
    /// RAII 清理通道（unbounded，send 是同步的）
    cleanup_tx: tokio::sync::mpsc::UnboundedSender<CleanupRequest>,
    /// K8s namespace（用于构建 K8s Service FQDN）
    namespace: String,
    /// K8s 集群域名
    cluster_domain: String,

    // === 反向索引（CQRS：写入时维护，读取时 O(1)） ===
    /// container_id → container_key（按容器 ID 快速查找）
    container_id_to_key: DashMap<String, String>,
    /// user_id → project_id 集合（多值索引）
    ///
    /// user_id 是 1:N（一个 user 可关联多个 project），用 DashSet 存全部 project_id。
    /// 服务于：
    /// - `find_projects_by_user_id`（cleanup 判断容器能否销毁需枚举该 user 的全部 project）
    /// - `find_by_user_id` / `get_container_by_user_id`（取任一即可，同 user 的同 ServiceType
    ///   项目共享同一容器）
    user_id_to_project_ids: DashMap<String, DashSet<String>>,
    /// pod_id → project_id（按 pod ID 快速查找）
    pod_id_to_project_id: DashMap<String, String>,
}

impl ProjectAdapter {
    /// 创建新的项目适配器
    ///
    /// 返回 (adapter, cleanup_receiver)。
    /// cleanup_receiver 需要传给 ResourceReaper 以处理容器销毁。
    pub fn new(namespace: String, cluster_domain: String) -> (Self, tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
        // logical_id（裸 logical id）：作 ContainerEntry.logical_id，供 RAII 清理 identifier 用。
        let logical_id = info.container_key().to_string();
        // DashMap 键：优先 container_name（跨重建稳定、含 service_type 前缀防跨类型碰撞），
        // 无容器信息时回退裸 logical_id（仅占位，不建容器条目，且 container_name 必含 prefix 不会与裸 id 相撞）。
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

        // 有容器信息时：Fail Fast 校验 service_type，然后增引用（键变）或刷新信息（键不变=重建）
        if let Some(container) = info.container() {
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
                // 键变了：增引用（新容器条目，或共享容器的多 project 复用）
                match self.containers.entry(key.clone()) {
                    Entry::Occupied(e) => {
                        e.get().inc_ref();
                    }
                    Entry::Vacant(e) => {
                        e.insert(Arc::new(ContainerEntry::new(
                            container.clone(),
                            st,
                            logical_id.clone(),
                        )));
                    }
                }
            } else {
                // 键不变但容器可能已重建（container_name 不变、container_id/ip 变）→ 刷新条目信息，
                // 否则 find_by_project_id/get_container 会返回旧 ip（容器重建陈旧问题）。
                // 同时清理旧 container_id 的反向索引，避免 container_id_to_key 累积陈旧条目。
                if let Some(entry) = self.containers.get(&key) {
                    let old_cid = entry.info().container_id;
                    if old_cid != container.container_id {
                        self.container_id_to_key.remove(&old_cid);
                        entry.update(container.clone(), st);
                    }
                }
            }
        }

        // 写入主存储和索引
        self.project_to_container
            .insert(project_id.clone(), key.clone());
        self.projects.insert(project_id.clone(), info.clone());

        // 维护反向索引：container_id → 容器键
        if let Some(container) = info.container() {
            self.container_id_to_key
                .insert(container.container_id.clone(), key);
        }
        // user_id → project_id 集合（多值）：entry API 原子插入，or_insert_with 的 RefMut
        // 在语句结束即释放，DashSet::insert 持的是 DashSet 内部锁，与 DashMap 分片锁互不嵌套。
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
    /// 容器引用归零时触发异步物理销毁。
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

    // ========== 容器操作 ==========

    /// 保存容器信息（更新或创建）
    ///
    /// 通过 view() 获取 Arc<ContainerEntry>，利用内部可变性（RwLock）更新。
    /// DashMap 读锁在 view() 闭包返回后立即释放，update() 仅持有 RwLock。
    /// 不存在时使用 entry() API 原子插入，避免 TOCTOU 竞态。
    ///
    /// # Errors
    /// 如果 `service_type` 为 `None`，返回错误（Fail Fast）。
    pub fn save_container(
        &self,
        container: &ContainerBasicInfo,
        service_type: Option<ServiceType>,
    ) -> anyhow::Result<()> {
        let st = match service_type {
            Some(st) => st,
            None => {
                tracing::error!(
                    "[STORAGE] service_type is None, cannot save container: container_id={}",
                    container.container_id
                );
                return Err(anyhow::anyhow!(
                    "service_type is required for save_container: container_id={}",
                    container.container_id
                ));
            }
        };

        // view() 获取 Arc（DashMap 读锁立即释放），通过 RwLock 更新字段。
        // 键用 container_name（与 insert 的 container_entry_key 一致）。
        let existing = self
            .containers
            .view(&container.container_name, |_, ce| ce.clone());

        match existing {
            Some(ce) => {
                ce.update(container.clone(), st);
            }
            None => {
                // 使用 entry() API 原子插入，避免 view()+insert() 的 TOCTOU 竞态
                match self.containers.entry(container.container_name.clone()) {
                    Entry::Occupied(e) => {
                        // 并发插入：另一个线程已插入，直接更新
                        e.get().update(container.clone(), st);
                    }
                    Entry::Vacant(e) => {
                        // logical_id 占位：save_container 早于 insert 时 logical_id 未知，
                        // 先用 container_id 占位，insert 会建带正确 logical_id 的条目。
                        e.insert(Arc::new(ContainerEntry::with_ref_count(
                            container.clone(),
                            st,
                            container.container_id.clone(),
                            0,
                        )));
                    }
                }
                // 维护反向索引：container_id → 容器键（container_name）
                self.container_id_to_key.insert(
                    container.container_id.clone(),
                    container.container_name.clone(),
                );
            }
        }
        Ok(())
    }

    /// 获取容器信息（按 container_id 查找，O(1) 索引查找）
    pub fn get_container(&self, container_id: &str) -> Option<ContainerBasicInfo> {
        let container_key = self.container_id_to_key.get(container_id)?;
        self.containers
            .view(container_key.value(), |_, ce| ce.info())
    }

    /// 删除容器及其关联的所有项目（RAII 触发物理销毁）
    ///
    /// 返回 (容器是否存在, 删除的项目数)
    pub fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize) {
        // 收集所有关联此容器的 project_id（通过索引 O(1) + O(n)）
        let container_key = self
            .container_id_to_key
            .get(container_id)
            .map(|r| r.value().clone());
        let project_ids: Vec<String> = match &container_key {
            Some(ck) => self
                .project_to_container
                .iter()
                .filter(|e| e.value() == ck)
                .map(|e| e.key().clone())
                .collect(),
            None => vec![],
        };

        let count = project_ids.len();

        // 逐个移除（每个 remove 都会 dec_ref）
        for pid in &project_ids {
            self.remove(pid);
        }

        // 如果容器条目还存在（没有 project 触发清理），用 entry 原子移除
        // 先通过索引 O(1) 查找 container_key
        let ck_to_remove: Option<String> = self
            .container_id_to_key
            .get(container_id)
            .map(|r| r.value().clone())
            .or_else(|| {
                // 索引未命中时回退到遍历（防御性）
                self.containers
                    .iter()
                    .find(|e| e.value().info().container_id == container_id)
                    .map(|e| e.key().clone())
            });

        let container_existed = ck_to_remove.is_some();
        if let Some(ck) = ck_to_remove
            && let Entry::Occupied(e) = self.containers.entry(ck)
        {
            let (_container_key, entry) = e.remove_entry();
            // 清理反向索引
            let info = entry.info();
            self.container_id_to_key.remove(&info.container_id);
            // identifier 用裸 logical_id（清理链路按 logical id）
            let _ = self.cleanup_tx.send(CleanupRequest {
                identifier: entry.logical_id().to_string(),
                container_name: info.container_name,
                service_type: entry.service_type(),
                container_ip: info.container_ip,
                namespace: self.namespace.clone(),
                cluster_domain: self.cluster_domain.clone(),
                project_ids,
            });
        }

        (container_existed, count)
    }

    /// 按服务类型获取所有容器
    pub fn get_containers_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> Vec<ContainerBasicInfo> {
        self.containers
            .iter()
            .filter(|e| e.value().service_type() == service_type)
            .map(|e| e.value().info())
            .collect()
    }

    /// 获取所有容器信息
    pub fn get_all_container_records(&self) -> Vec<ContainerBasicInfo> {
        self.containers.iter().map(|e| e.value().info()).collect()
    }

    /// 根据 container_id 获取关联的项目列表
    ///
    /// 先通过 `container_id_to_key` 索引找到 container_key，
    /// 再通过 `project_to_container` 反向索引找到关联的 project_id。
    pub fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        // 通过索引找到 container_key
        let container_key = match self.container_id_to_key.get(container_id) {
            Some(r) => r.value().clone(),
            None => return vec![],
        };
        // 通过 project_to_container 反向索引找到关联的 project_id
        self.project_to_container
            .iter()
            .filter(|e| e.value() == &container_key)
            .filter_map(|e| self.projects.view(e.key(), |_, v| v.clone()))
            .collect()
    }

    // ========== ComputerAgentRunner 模式 ==========

    /// 通过 user_id 获取容器信息（需指定 service_type）
    ///
    /// user_id 是 1:N（一个 user 可有多个 project），走多值索引 `find_projects_by_user_id`
    /// 取任一匹配 project 的容器——同 user 同 ServiceType 的项目共享同一容器，任取一个即可。
    pub fn get_container_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Option<ContainerBasicInfo> {
        self.find_projects_by_user_id(user_id, service_type)
            .into_iter()
            .next()
            .and_then(|p| p.container().cloned())
    }

    /// 通过 pod_id 获取容器信息（O(1) 索引查找）
    pub fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        let project_id = self.pod_id_to_project_id.get(pod_id)?;
        let info = self.projects.view(project_id.value(), |_, v| v.clone())?;
        info.container().cloned()
    }

    /// 通过 user_id 查找所有项目（按 service_type 过滤）
    ///
    /// 走多值索引 `user_id_to_project_ids`（user_id → 全部 project_id），再逐个解析并按
    /// `service_type` 过滤。复杂度 O(该 user 的 project 数)，优于全量遍历。
    ///
    /// 用途：cleanup strategy 判断容器能否销毁时，需枚举该 user 的全部同 ServiceType
    /// project 检查闲置状态；`find_by_user_id` / `get_container_by_user_id` 也委托本方法
    /// 取任一（同 user 同 ServiceType 项目共享同一容器）。
    pub fn find_projects_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        // 先从多值索引取出该 user 的全部 project_id（Ref 在语句结束释放）
        let project_ids: Vec<String> = match self.user_id_to_project_ids.get(user_id) {
            Some(set_ref) => set_ref.iter().map(|e| e.key().clone()).collect(),
            None => return vec![],
        };
        // 逐个解析 project，按 service_type 过滤
        project_ids
            .into_iter()
            .filter_map(|pid| self.projects.view(&pid, |_, v| v.clone()))
            .filter(|p| p.service_type().as_ref() == Some(service_type))
            .collect()
    }

    /// 通过 pod_id 查找所有项目
    ///
    /// **必须全量遍历**：cleanup strategy 用本方法判断共享容器（pod_id）是否有活跃引用，
    /// 必须返回所有同 pod 的 project。`pod_id_to_project_id` 索引只存最后插入的 project_id
    /// （insert 时覆盖），无法返回全部，所以这里用全量遍历（与 `find_projects_by_user_id` 对称）。
    ///
    /// O(N) 遍历，N 是 project 总数。N 通常很小（参考 `find_projects_by_user_id` 的 m4 文档）。
    pub fn find_projects_by_pod_id(&self, pod_id: &str) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.projects
            .iter()
            .filter(|e| e.value().pod_id() == Some(pod_id))
            .map(|e| e.value().clone())
            .collect()
    }

    // ========== 清理相关 ==========

    /// 查找空闲容器
    pub fn find_idle_containers(
        &self,
        idle_minutes: i64,
        _protection_minutes: i64,
    ) -> Vec<IdleContainerInfo> {
        // 第一步：从 containers 收集数据（iter 读锁在 collect 后释放）
        let container_data: Vec<_> = self
            .containers
            .iter()
            .filter(|e| e.value().is_idle(idle_minutes))
            .map(|e| {
                (
                    e.value().info(),
                    e.value().service_type(),
                    e.value().last_activity(),
                )
            })
            .collect();

        // 第二步：从 projects 收集关联关系（containers 锁已释放，无死锁风险）
        container_data
            .into_iter()
            .map(|(info, service_type, last_activity)| {
                let project_ids: Vec<String> = self
                    .projects
                    .iter()
                    .filter(|p| {
                        p.container()
                            .map(|c| c.container_id == info.container_id)
                            .unwrap_or(false)
                    })
                    .map(|p| p.key().clone())
                    .collect();
                let idle_mins = Utc::now()
                    .signed_duration_since(last_activity)
                    .num_minutes()
                    .abs();
                IdleContainerInfo {
                    container_id: info.container_id,
                    container_name: info.container_name,
                    service_type,
                    idle_minutes: idle_mins,
                    project_ids,
                }
            })
            .collect()
    }

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

    /// 减少容器引用计数，归零时触发 RAII 清理
    ///
    /// 使用 entry() API 实现 dec_ref + remove 的原子操作，
    /// 消除 view() + remove() 之间的 TOCTOU 竞态。
    fn dec_container_ref(&self, container_key: &str) {
        let entry = match self.containers.entry(container_key.to_string()) {
            Entry::Occupied(e) => e,
            Entry::Vacant(_) => return,
        };

        // dec_ref 在 entry 写锁范围内，与后续 remove_entry 原子
        let remaining = entry.get().dec_ref();
        if remaining == 0 {
            let (_ck, entry) = entry.remove_entry();
            let info = entry.info();
            // 清理反向索引
            self.container_id_to_key.remove(&info.container_id);
            info!(
                "[STORAGE] RAII: container refcount=0, sending cleanup for {}",
                info.container_name
            );
            // identifier 用裸 logical_id（清理链路按 logical id：stop_container_by_identifier/
            // remove_vnc_backend/remove_project_backend/remove_container_cache），而非 DashMap 键
            let _ = self.cleanup_tx.send(CleanupRequest {
                identifier: entry.logical_id().to_string(),
                container_name: info.container_name,
                service_type: entry.service_type(),
                container_ip: info.container_ip,
                namespace: self.namespace.clone(),
                cluster_domain: self.cluster_domain.clone(),
                project_ids: vec![],
            });
        }
    }
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
    match info.container() {
        Some(c) => c.container_name.clone(),
        None => info.container_key().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::ProjectExtendedFields;

    /// 测试用的 K8s namespace
    const TEST_NAMESPACE: &str = "test-namespace";
    /// 测试用的 K8s 集群域名
    const TEST_CLUSTER_DOMAIN: &str = "test.cluster.local";

    fn create_test_info(project_id: &str) -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new(project_id.to_string());
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        info
    }

    fn create_test_info_with_container(
        project_id: &str,
        container_name: &str,
    ) -> ProjectAndContainerInfo {
        let mut info = create_test_info(project_id);
        info.set_container(Some(ContainerBasicInfo {
            container_id: format!("{}-id", container_name),
            container_name: container_name.to_string(),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{}", container_name),
        }));
        info
    }

    fn make_adapter() -> ProjectAdapter {
        let (adapter, _) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        adapter
    }

    #[test]
    fn test_project_crud() {
        let adapter = make_adapter();
        let project_id = "test-project-1";
        let info = Arc::new(create_test_info(project_id));

        // insert
        adapter
            .insert(project_id.to_string(), info.clone())
            .unwrap();
        assert!(adapter.contains_key(project_id));

        // get
        let retrieved = adapter.get(project_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().project_id(), project_id);

        // remove
        let removed = adapter.remove(project_id);
        assert!(removed.is_some());
        assert!(!adapter.contains_key(project_id));
    }

    #[test]
    fn test_session_operations() {
        let adapter = make_adapter();
        let project_id = "test-project-2";
        let session_id = "test-session-1";

        let info = Arc::new(create_test_info_with_container(project_id, "container-1"));
        adapter.insert(project_id.to_string(), info).unwrap();

        // C1 修复后的推荐路径：add_session_to_project 单步原子
        let added = adapter.add_session_to_project(project_id, session_id);
        assert!(added, "add_session_to_project 应在 project 存在时返回 true");

        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some());
        assert_eq!(by_session.unwrap().project_id(), project_id);

        let container_name = adapter.get_container_name_by_session(session_id);
        assert_eq!(container_name, Some("container-1".to_string()));

        // 不存在的 project 应返回 false
        let added2 = adapter.add_session_to_project("nonexistent", session_id);
        assert!(!added2);
    }

    #[test]
    fn test_iter() {
        let adapter = make_adapter();
        for i in 0..3 {
            let pid = format!("iter-project-{}", i);
            adapter
                .insert(pid.clone(), Arc::new(create_test_info(&pid)))
                .unwrap();
        }
        assert_eq!(adapter.iter().len(), 3);
    }

    #[test]
    fn test_insert_with_session() {
        let adapter = make_adapter();
        let project_id = "test-project-session";
        let session_id = "test-session-abc";
        let info = Arc::new(create_test_info(project_id));

        adapter
            .insert_with_session(project_id.to_string(), info, Some(session_id))
            .unwrap();

        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some());
        assert_eq!(by_session.unwrap().project_id(), project_id);

        adapter.clear_session(project_id);
        assert!(adapter.get_by_session_id(session_id).is_none());
    }

    /// C2 修复后的新语义：多 session 共存（不再覆盖）
    ///
    /// 注意：insert_with_session 接收的 `info` 会覆盖主存储中的 ProjectAndContainerInfo。
    /// 生产代码（computer_chat_handler.rs:1123-1132）在调用前会先读出 existing info 并迁移 sessions。
    /// 本测试模拟该正确用法。
    #[test]
    fn test_session_rotation() {
        let adapter = make_adapter();
        let project_id = "test-rotation";
        let info = Arc::new(create_test_info(project_id));

        // 第一次：插入 info 并关联 session-1
        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some("session-1"))
            .unwrap();
        assert!(adapter.get_by_session_id("session-1").is_some());

        // 模拟生产用法：读出 existing info，迁移已有 sessions，添加新 session
        let mut updated_info = adapter.get(project_id).unwrap().as_ref().clone();
        updated_info.add_session("session-2");
        adapter
            .insert_with_session(
                project_id.to_string(),
                Arc::new(updated_info),
                Some("session-2"),
            )
            .unwrap();

        assert!(adapter.get_by_session_id("session-2").is_some());
        // C2 关键断言：session-1 仍然可查（多窗口场景）
        assert!(
            adapter.get_by_session_id("session-1").is_some(),
            "C2: 新 session 加入后旧 session 应仍可查"
        );

        // latest_session 应指向最新加入的 session-2
        let info = adapter.get(project_id).unwrap();
        assert_eq!(info.latest_session(), Some("session-2"));
        assert_eq!(info.session_count(), 2);
    }

    /// 多 session：add_session_to_project + clear_session_one 保留其他
    #[test]
    fn test_multi_session_add_and_clear_one() {
        let adapter = make_adapter();
        let project_id = "test-multi";
        let info = Arc::new(create_test_info(project_id));
        adapter.insert(project_id.to_string(), info).unwrap();

        // 添加 3 个 session
        adapter.add_session_to_project(project_id, "s1");
        adapter.add_session_to_project(project_id, "s2");
        adapter.add_session_to_project(project_id, "s3");

        // 3 个都可查
        assert!(adapter.get_by_session_id("s1").is_some());
        assert!(adapter.get_by_session_id("s2").is_some());
        assert!(adapter.get_by_session_id("s3").is_some());

        let info = adapter.get(project_id).unwrap();
        assert_eq!(info.session_count(), 3);
        assert_eq!(info.latest_session(), Some("s3"));

        // 清单个 session（保留其他）
        let cleared = adapter.clear_session_one(project_id, "s2");
        assert!(cleared, "clear_session_one 应在 session 存在时返回 true");
        assert!(adapter.get_by_session_id("s2").is_none(), "s2 应被清");
        assert!(adapter.get_by_session_id("s1").is_some(), "s1 应保留");
        assert!(adapter.get_by_session_id("s3").is_some(), "s3 应保留");

        let info = adapter.get(project_id).unwrap();
        assert_eq!(info.session_count(), 2);

        // 清不存在的 session 返回 false
        let cleared2 = adapter.clear_session_one(project_id, "nonexistent");
        assert!(!cleared2);
    }

    /// clear_session（清所有）+ remove_project 自动清理所有 session 索引
    #[test]
    fn test_clear_all_sessions_and_remove() {
        let adapter = make_adapter();
        let project_id = "test-clear-all";
        let info = Arc::new(create_test_info(project_id));
        adapter.insert(project_id.to_string(), info).unwrap();

        adapter.add_session_to_project(project_id, "s1");
        adapter.add_session_to_project(project_id, "s2");
        assert_eq!(adapter.session_index.len(), 2);

        // clear_session 清所有
        adapter.clear_session(project_id);
        assert_eq!(
            adapter.session_index.len(),
            0,
            "clear_session 应清所有 session 索引"
        );
        let info = adapter.get(project_id).unwrap();
        assert_eq!(info.session_count(), 0);
        assert!(info.latest_session().is_none());

        // 重新添加 + remove 项目
        adapter.add_session_to_project(project_id, "s3");
        adapter.add_session_to_project(project_id, "s4");
        assert_eq!(adapter.session_index.len(), 2);

        let _ = adapter.remove(project_id);
        assert_eq!(
            adapter.session_index.len(),
            0,
            "remove 应清理所有 session 索引"
        );
    }

    /// latest_session 在 remove latest 后自动退化到剩余 session
    #[test]
    fn test_latest_session_fallback_after_remove() {
        let adapter = make_adapter();
        let project_id = "test-latest-fallback";
        let info = Arc::new(create_test_info(project_id));
        adapter.insert(project_id.to_string(), info).unwrap();

        adapter.add_session_to_project(project_id, "s1");
        adapter.add_session_to_project(project_id, "s2");
        // latest 是 s2

        adapter.clear_session_one(project_id, "s2");
        let info = adapter.get(project_id).unwrap();
        assert_eq!(info.session_count(), 1);
        // latest 退化到剩余的 s1
        assert_eq!(
            info.latest_session(),
            Some("s1"),
            "移除 latest 后应退化到剩余 session"
        );
    }

    /// 并发压测：8 线程 × 200 轮 add_session + clear_session_one，无 panic/deadlock
    #[test]
    fn test_concurrent_multi_session_add_and_clear() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        const THREADS: usize = 8;
        const ITERS: usize = 200;

        // 预插入 project
        let info = Arc::new(create_test_info("proj-concurrent"));
        adapter.insert("proj-concurrent".to_string(), info).unwrap();

        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for t in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERS {
                    let sid = format!("t{}-s{}", t, i);
                    adapter.add_session_to_project("proj-concurrent", &sid);
                    // 50% 概率清掉自己刚加的
                    if i % 2 == 0 {
                        adapter.clear_session_one("proj-concurrent", &sid);
                    }
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 15);
            assert!(result.is_some(), "DEADLOCK: concurrent multi-session ops");
        }

        // 最终 session_count 应 = 偶数 iter 数量之和（i%2==1 的没被清）
        let info = adapter.get("proj-concurrent").unwrap();
        let expected: usize = THREADS * (ITERS / 2);
        assert_eq!(
            info.session_count(),
            expected,
            "残留 session 数应等于未清理的数量"
        );
    }

    #[test]
    fn test_len_and_is_empty() {
        let adapter = make_adapter();
        assert!(adapter.is_empty());
        assert_eq!(adapter.len(), 0);

        adapter
            .insert("p1".to_string(), Arc::new(create_test_info("p1")))
            .unwrap();
        assert_eq!(adapter.len(), 1);
        assert!(!adapter.is_empty());
    }

    #[test]
    fn test_update_activity() {
        let adapter = make_adapter();
        let pid = "test-activity";
        adapter
            .insert(pid.to_string(), Arc::new(create_test_info(pid)))
            .unwrap();

        let ts = adapter.update_activity(pid);
        assert!(ts.is_some());

        // 不存在的 project
        assert!(adapter.update_activity("nonexistent").is_none());
    }

    #[test]
    fn test_get_stats() {
        let adapter = make_adapter();
        let stats = adapter.get_stats();
        assert_eq!(stats.total_projects, 0);
        assert_eq!(stats.total_containers, 0);
        assert_eq!(stats.active_sessions, 0);
    }

    #[test]
    fn test_dump_summary() {
        let adapter = make_adapter();
        let summary = adapter.dump_summary();
        assert!(summary.contains("projects=0"));
    }

    #[test]
    fn test_raii_cleanup_on_last_project_remove() {
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "shared-container-id".to_string(),
            container_name: "shared-container".to_string(),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        let mut info1 = ProjectAndContainerInfo::from_parts(
            "proj-1".to_string(),
            Some("user-1".to_string()),
            None,
            None,
            Some(container.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info1.set_service_type(Some(ServiceType::ComputerAgentRunner));

        let mut info2 = ProjectAndContainerInfo::from_parts(
            "proj-2".to_string(),
            Some("user-1".to_string()),
            None,
            None,
            Some(container.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info2.set_service_type(Some(ServiceType::ComputerAgentRunner));

        adapter
            .insert("proj-1".to_string(), Arc::new(info1))
            .unwrap();
        adapter
            .insert("proj-2".to_string(), Arc::new(info2))
            .unwrap();

        assert_eq!(adapter.containers.len(), 1);

        adapter.remove("proj-1");
        assert_eq!(
            adapter.containers.len(),
            1,
            "容器应保留（ref_count 应 > 0）"
        );

        adapter.remove("proj-2");
        assert_eq!(
            adapter.containers.len(),
            0,
            "容器应已销毁（ref_count = 0 触发 RAII）"
        );
    }

    #[test]
    fn test_reinsert_same_project_no_ref_leak() {
        let adapter = make_adapter();

        let info = Arc::new(create_test_info_with_container("proj-A", "container-A"));

        adapter.insert("proj-A".to_string(), info.clone()).unwrap();
        assert_eq!(adapter.containers.len(), 1);

        adapter.insert("proj-A".to_string(), info.clone()).unwrap();
        assert_eq!(adapter.containers.len(), 1);

        adapter.remove("proj-A");
        assert_eq!(
            adapter.containers.len(),
            0,
            "重复 insert 不应导致 ref_count 泄露，remove 后容器应被清理"
        );
    }

    #[test]
    fn test_save_container_update() {
        let adapter = make_adapter();

        // containers DashMap 以 container_name 为键；save_container 与 insert 都用 container_name，
        // 故此处 container_name 与 insert 的 info.container().container_name 必须一致才能命中同一条目。
        let container = ContainerBasicInfo {
            container_id: "proj-1".to_string(),
            container_name: "save-test".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        // 第一次 save：创建新条目（ref_count=0）
        adapter
            .save_container(&container, Some(ServiceType::WebAgentRunner))
            .unwrap();
        assert_eq!(adapter.containers.len(), 1);

        // 通过 project insert 关联容器（ref_count 0→1）
        let mut info = create_test_info("proj-1");
        info.set_container(Some(container.clone()));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // 验证 ref_count = 1（键为 container_name "save-test"）
        let ce = adapter.containers.get("save-test").unwrap();
        assert_eq!(ce.value().ref_count(), 1);

        // 第二次 save：更新已有条目（保持 container_name 不变以命中同一条目），ref_count 应保持不变
        let mut updated_container = container.clone();
        updated_container.container_ip = "10.0.0.2".to_string();
        adapter
            .save_container(&updated_container, Some(ServiceType::ComputerAgentRunner))
            .unwrap();

        let ce = adapter.containers.get("save-test").unwrap();
        assert_eq!(
            ce.value().ref_count(),
            1,
            "save_container 更新不应改变 ref_count"
        );
        assert_eq!(ce.value().info().container_ip, "10.0.0.2");
        assert_eq!(ce.value().service_type(), ServiceType::ComputerAgentRunner);
    }

    // ========== 并发 RAII + 死锁验证测试 ==========

    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant};

    fn join_with_timeout<T>(handle: thread::JoinHandle<T>, timeout_secs: u64) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        while !handle.is_finished() {
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().ok()
    }

    fn drain_cleanup_requests(
        rx: &std::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>>,
    ) -> Vec<CleanupRequest> {
        let mut guard = rx.lock().unwrap();
        let mut requests = vec![];
        while let Ok(req) = guard.try_recv() {
            requests.push(req);
        }
        requests
    }

    fn create_shared_project(
        project_id: &str,
        user_id: &str,
        container: &ContainerBasicInfo,
    ) -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::from_parts(
            project_id.to_string(),
            Some(user_id.to_string()),
            None,
            None,
            Some(container.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        info
    }

    #[test]
    fn test_concurrent_insert_remove_no_deadlock() {
        let (adapter, rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);
        let rx = Arc::new(std::sync::Mutex::new(rx));

        const THREADS: usize = 8;
        const ITERS: usize = 50;
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for t in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERS {
                    let pid = format!("t{}-i{}", t, i);
                    let info = Arc::new(create_test_info_with_container(
                        &pid,
                        &format!("c-t{}-i{}", t, i),
                    ));
                    let _ = adapter.insert(pid.clone(), info);
                    let _ = adapter.remove(&pid);
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 15);
            assert!(
                result.is_some(),
                "DEADLOCK: thread did not complete within 15s"
            );
        }

        assert_eq!(adapter.len(), 0, "all projects should be removed");
        assert_eq!(
            adapter.containers.len(),
            0,
            "all containers should be cleaned via RAII"
        );

        let cleanups = drain_cleanup_requests(&rx);
        assert_eq!(
            cleanups.len(),
            THREADS * ITERS,
            "RAII should send one cleanup per container"
        );
    }

    #[test]
    fn test_concurrent_same_project_insert_remove() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        const THREADS: usize = 8;
        const ITERS: usize = 200;
        let project_id = "shared-project";
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for _ in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERS {
                    let info = Arc::new(create_test_info_with_container(
                        project_id,
                        "shared-container",
                    ));
                    let _ = adapter.insert(project_id.to_string(), info);
                    let _ = adapter.remove(project_id);
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 15);
            assert!(
                result.is_some(),
                "DEADLOCK: concurrent insert/remove of same project_id"
            );
        }
    }

    #[test]
    fn test_concurrent_shared_container_remove() {
        let (adapter, rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);
        let rx = Arc::new(std::sync::Mutex::new(rx));

        let container = ContainerBasicInfo {
            container_id: "shared-id".to_string(),
            container_name: "shared-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        let info1 = create_shared_project("proj-1", "user-1", &container);
        let info2 = create_shared_project("proj-2", "user-1", &container);

        adapter
            .insert("proj-1".to_string(), Arc::new(info1))
            .unwrap();
        adapter
            .insert("proj-2".to_string(), Arc::new(info2))
            .unwrap();

        assert_eq!(adapter.containers.len(), 1);

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = vec![];

        for pid in ["proj-1", "proj-2"] {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                adapter.remove(pid)
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 10);
            assert!(
                result.is_some(),
                "DEADLOCK: concurrent remove of shared container projects"
            );
        }

        assert_eq!(adapter.len(), 0);
        assert_eq!(
            adapter.containers.len(),
            0,
            "container should be cleaned up"
        );

        let cleanups = drain_cleanup_requests(&rx);
        assert_eq!(
            cleanups.len(),
            1,
            "RAII should send exactly 1 cleanup for shared container"
        );
        assert_eq!(cleanups[0].identifier, "user-1");
        assert_eq!(cleanups[0].container_ip, "10.0.0.1");
    }

    #[test]
    fn test_concurrent_session_update_and_remove() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        let pid = "concurrent-session-proj";
        let info = Arc::new(create_test_info_with_container(pid, "session-container"));
        adapter.insert(pid.to_string(), info).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = vec![];

        {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..100 {
                    let sid = format!("session-{}", i);
                    // C2 修复后改用 add_session_to_project（多 session 模型）
                    adapter.add_session_to_project(pid, &sid);
                }
            }));
        }

        {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                thread::sleep(Duration::from_millis(5));
                let _ = adapter.remove(pid);
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 10);
            assert!(
                result.is_some(),
                "DEADLOCK: concurrent session update and remove"
            );
        }
    }

    #[test]
    fn test_concurrent_insert_with_session_and_remove() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        const THREADS: usize = 4;
        const ITERS: usize = 50;
        let pid = "session-battle";
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for t in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERS {
                    let info = Arc::new(create_test_info_with_container(pid, "battle-c"));
                    let sid = format!("sid-{}-{}", t, i);
                    let _ = adapter.insert_with_session(pid.to_string(), info, Some(&sid));
                    let _ = adapter.remove(pid);
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 15);
            assert!(
                result.is_some(),
                "DEADLOCK: concurrent insert_with_session and remove"
            );
        }
    }

    #[test]
    fn test_concurrent_remove_nonexistent() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        const THREADS: usize = 8;
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for _ in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..100 {
                    let result = adapter.remove("nonexistent-project");
                    assert!(result.is_none(), "removing nonexistent should return None");
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 10);
            assert!(
                result.is_some(),
                "DEADLOCK: concurrent remove of nonexistent project"
            );
        }
    }

    #[test]
    fn test_concurrent_stress_mixed_operations() {
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let adapter = Arc::new(adapter);

        for i in 0..10 {
            let pid = format!("preload-{}", i);
            let info = Arc::new(create_test_info_with_container(
                &pid,
                &format!("c-pre-{}", i),
            ));
            adapter.insert(pid, info).unwrap();
        }

        const THREADS: usize = 8;
        const ITERS: usize = 30;
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = vec![];

        for t in 0..THREADS {
            let adapter = adapter.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERS {
                    let pid = format!("stress-{}-{}", t, i);
                    let info = Arc::new(create_test_info_with_container(
                        &pid,
                        &format!("sc-{}-{}", t, i),
                    ));

                    let _ = adapter.insert(pid.clone(), info);
                    let _ = adapter.get(&pid);
                    let _ = adapter.update_activity(&pid);

                    let sid = format!("sid-{}-{}", t, i);
                    adapter.add_session_to_project(&pid, &sid);
                    let _ = adapter.get_by_session_id(&sid);
                    let _ = adapter.get_container_name_by_session(&sid);

                    adapter.clear_session(&pid);
                    let _ = adapter.remove(&pid);
                }
            }));
        }

        for h in handles {
            let result = join_with_timeout(h, 15);
            assert!(
                result.is_some(),
                "DEADLOCK: stress test with mixed operations"
            );
        }
    }

    #[test]
    fn test_raii_cleanup_request_content() {
        let (adapter, rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let rx = Arc::new(std::sync::Mutex::new(rx));

        let info = Arc::new(create_test_info_with_container("proj-verify", "c-verify"));
        adapter.insert("proj-verify".to_string(), info).unwrap();

        let removed = adapter.remove("proj-verify");
        assert!(removed.is_some());

        let cleanups = drain_cleanup_requests(&rx);
        assert_eq!(cleanups.len(), 1);

        let req = &cleanups[0];
        assert_eq!(req.identifier, "proj-verify");
        assert_eq!(req.container_name, "c-verify");
        assert_eq!(req.container_ip, "127.0.0.1");
        assert_eq!(req.service_type, ServiceType::WebAgentRunner);
    }

    #[test]
    fn test_shared_container_ref_count_no_leak_under_reinsert() {
        let (adapter, rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
        let rx = Arc::new(std::sync::Mutex::new(rx));

        let container = ContainerBasicInfo {
            container_id: "leak-test-id".to_string(),
            container_name: "leak-test".to_string(),
            container_ip: "10.0.0.5".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://leak".to_string(),
        };

        for round in 0..5 {
            let info1 = create_shared_project("proj-1", "user-leak", &container);
            let info2 = create_shared_project("proj-2", "user-leak", &container);

            adapter
                .insert("proj-1".to_string(), Arc::new(info1))
                .unwrap();
            adapter
                .insert("proj-2".to_string(), Arc::new(info2))
                .unwrap();

            assert_eq!(
                adapter.containers.len(),
                1,
                "round {}: should have 1 container",
                round
            );

            adapter.remove("proj-1");
            assert_eq!(
                adapter.containers.len(),
                1,
                "round {}: container should persist after removing proj-1",
                round
            );

            adapter.remove("proj-2");
            assert_eq!(
                adapter.containers.len(),
                0,
                "round {}: container should be cleaned after removing last project",
                round
            );
        }

        let cleanups = drain_cleanup_requests(&rx);
        assert_eq!(
            cleanups.len(),
            5,
            "5 rounds should produce 5 cleanup requests"
        );
    }

    // ========== 索引一致性测试 ==========

    #[test]
    fn test_index_container_id_lookup() {
        // save_container + insert 后，get_container 通过索引 O(1) 查找
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-123".to_string(),
            container_name: "test-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        // save_container 注册容器
        adapter
            .save_container(&container, Some(ServiceType::WebAgentRunner))
            .unwrap();

        // insert 关联项目（container_entry_key = container_name，与 save_container 同键）
        let mut info = create_test_info("proj-1");
        info.set_container(Some(container.clone()));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // get_container 通过索引查找
        let found = adapter.get_container("cid-123");
        assert!(found.is_some(), "get_container 应通过索引找到容器");
        assert_eq!(found.unwrap().container_id, "cid-123");

        // 不存在的 container_id
        assert!(adapter.get_container("nonexistent").is_none());
    }

    #[test]
    fn test_index_user_id_lookup() {
        // insert 后，get_container_by_user_id 通过索引 O(1) 查找
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-user-1".to_string(),
            container_name: "user-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        let mut info = ProjectAndContainerInfo::from_parts(
            "proj-1".to_string(),
            Some("user-abc".to_string()),
            None,
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // 通过 user_id 查找容器
        let found = adapter.get_container_by_user_id("user-abc", &ServiceType::ComputerAgentRunner);
        assert!(
            found.is_some(),
            "get_container_by_user_id 应通过索引找到容器"
        );
        assert_eq!(found.unwrap().container_id, "cid-user-1");

        // 不存在的 user_id
        assert!(adapter
            .get_container_by_user_id("nonexistent", &ServiceType::ComputerAgentRunner)
            .is_none());
    }

    /// 回归测试：同 user_id 下不同 ServiceType 项目按 service_type 隔离查找
    ///
    /// 场景：user_id=6 同时存在 Computer（proj-A）和 Web（proj-B）两个业务。
    /// 多值索引 `user_id_to_project_ids` 同时收录两类项目的 project_id（信息完整），
    /// 查询侧（find_by_user_id / find_projects_by_user_id）按 service_type 过滤，
    /// 确保不串用、且 Web 项目不计入 Computer 容器的清理决策。
    #[test]
    fn test_user_id_index_not_polluted_by_web_project() {
        use shared_types::ContainerLookup;
        let adapter = make_adapter();

        let mk_container = |cid: &str, ip: &str, pid: &str| ContainerBasicInfo {
            container_id: cid.to_string(),
            container_name: format!("container-{}", cid),
            container_ip: ip.to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: pid.to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{}", cid),
        };

        // Computer 项目（user_id 索引消费者）
        let mut comp = ProjectAndContainerInfo::from_parts(
            "proj-A".to_string(),
            Some("user-6".to_string()),
            None,
            None,
            Some(mk_container("cid-comp", "10.0.0.1", "proj-A")),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        comp.set_service_type(Some(ServiceType::ComputerAgentRunner));
        adapter
            .insert("proj-A".to_string(), Arc::new(comp))
            .unwrap();

        // Web 项目（同一 user_id=6，模拟 pod_ensure 对 Web 也 set_user_id）
        let mut web = ProjectAndContainerInfo::from_parts(
            "proj-B".to_string(),
            Some("user-6".to_string()),
            None,
            None,
            Some(mk_container("cid-web", "10.0.0.2", "proj-B")),
            ProjectExtendedFields {
                service_type: Some(ServiceType::WebAgentRunner),
                ..Default::default()
            },
        );
        web.set_service_type(Some(ServiceType::WebAgentRunner));
        adapter
            .insert("proj-B".to_string(), Arc::new(web))
            .unwrap();

        // 关键断言 1：多值索引 user-6 同时收录两类业务的 project（信息完整）
        let collected: Vec<String> = adapter
            .user_id_to_project_ids
            .get("user-6")
            .map(|s| s.iter().map(|e| e.key().clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            collected
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>(),
            ["proj-A".to_string(), "proj-B".to_string()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            "多值索引应同时收录 Computer 和 Web 项目（同 user_id）"
        );

        // 关键断言 2：find_by_user_id("6", Computer) → Computer 容器 IP（按 service_type 过滤，不串到 Web）
        assert_eq!(
            adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner),
            Some("10.0.0.1".to_string()),
            "Computer 查找应命中 Computer 容器，不被同 user 的 Web 项目影响"
        );

        // 关键断言 3：find_projects_by_user_id 按 service_type 过滤
        // Web 项目虽记录了 user_id，但不应计入 Computer 的项目集合（避免 cleanup 误保活）
        let comp_projects = adapter.find_projects_by_user_id("user-6", &ServiceType::ComputerAgentRunner);
        assert_eq!(
            comp_projects.iter().map(|p| p.project_id()).collect::<Vec<_>>(),
            vec!["proj-A"],
            "find_projects_by_user_id(Computer) 应只返回 Computer 项目"
        );
        let web_projects = adapter.find_projects_by_user_id("user-6", &ServiceType::WebAgentRunner);
        assert_eq!(
            web_projects.iter().map(|p| p.project_id()).collect::<Vec<_>>(),
            vec!["proj-B"],
            "find_projects_by_user_id(Web) 应只返回 Web 项目"
        );

        // 关键断言 4：删除 Web 项目后，user-6 的索引集合应只剩 proj-A（Computer）
        adapter.remove("proj-B");
        let remaining: Vec<String> = adapter
            .user_id_to_project_ids
            .get("user-6")
            .map(|s| s.iter().map(|e| e.key().clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            remaining,
            vec!["proj-A".to_string()],
            "删除 Web 项目后，user-6 索引集合应只剩 Computer 项目 proj-A"
        );
        // 且 Computer 查找仍正常
        assert_eq!(
            adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner),
            Some("10.0.0.1".to_string()),
            "删除 Web 项目后，Computer 查找应仍命中 Computer 容器"
        );
    }

    /// 验证 user_id 索引单值限制（诊断用）：
    /// user 6 有两个 Computer 项目 proj-A/proj-C（共享同一容器，refcount=2）。
    /// user_id 索引单值，指向最后插入的 proj-C。删除 proj-C 后索引被清，
    /// 但 proj-A 仍引用容器（refcount=1）——此时 find_by_user_id 应仍能找到容器。
    #[test]
    fn test_find_by_user_id_after_indexed_project_removed() {
        use shared_types::ContainerLookup;
        let adapter = make_adapter();

        let mk_container = || ContainerBasicInfo {
            container_id: "cid-shared".to_string(),
            container_name: "computer-container".to_string(),
            container_ip: "10.0.0.9".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        let mk_proj = |pid: &str| {
            let mut p = ProjectAndContainerInfo::from_parts(
                pid.to_string(),
                Some("user-6".to_string()),
                None,
                None,
                Some(mk_container()),
                ProjectExtendedFields {
                    service_type: Some(ServiceType::ComputerAgentRunner),
                    ..Default::default()
                },
            );
            p.set_service_type(Some(ServiceType::ComputerAgentRunner));
            p
        };

        adapter.insert("proj-A".to_string(), Arc::new(mk_proj("proj-A"))).unwrap();
        adapter.insert("proj-C".to_string(), Arc::new(mk_proj("proj-C"))).unwrap();

        // 两项目共享同一容器条目（键为 container_name "computer-container"）
        assert_eq!(adapter.containers.len(), 1, "两个 Computer 项目应共享同一容器条目");
        assert_eq!(
            adapter.containers.get("computer-container").unwrap().ref_count(),
            2
        );

        // 删除 proj-C：容器仍存活（proj-A 引用，refcount=1）
        adapter.remove("proj-C");
        assert_eq!(adapter.containers.len(), 1, "容器应仍存活（proj-A 引用）");
        assert_eq!(
            adapter.containers.get("computer-container").unwrap().ref_count(),
            1
        );

        // find_by_user_id 走 find_projects_by_user_id 扫描，proj-A 仍引用容器 → 应仍能找到
        let result = adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner);
        assert_eq!(
            result,
            Some("10.0.0.9".to_string()),
            "删除 proj-C 后，user 6 仍有 proj-A 引用容器，find_by_user_id 应能找到"
        );
    }

    /// Computer pod_id 共享容器：不同 user 通过同一 pod_id 共享一个容器。
    /// 验证 container_key=pod_id → 共享容器条目（refcount）+ RAII 正确。
    #[test]
    fn test_computer_pod_id_shared_container() {
        let adapter = make_adapter();

        let shared = ContainerBasicInfo {
            container_id: "cid-shared-pod".to_string(),
            container_name: "computer-shared".to_string(),
            container_ip: "10.0.0.7".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        // user-A、user-B 各自一个 Computer 项目，通过 pod_id="pod-shared" 共享容器
        let mk_proj = |pid: &str, uid: &str| {
            let mut p = ProjectAndContainerInfo::from_parts(
                pid.to_string(),
                Some(uid.to_string()),
                Some("pod-shared".to_string()),
                None,
                Some(shared.clone()),
                ProjectExtendedFields {
                    service_type: Some(ServiceType::ComputerAgentRunner),
                    ..Default::default()
                },
            );
            p.set_service_type(Some(ServiceType::ComputerAgentRunner));
            p
        };

        adapter.insert("proj-A".to_string(), Arc::new(mk_proj("proj-A", "user-A"))).unwrap();
        // container_key = pod_id（Computer 有 pod_id 时），故与 user-B 共享同一容器条目
        assert_eq!(
            adapter.get("proj-A").unwrap().container_key(),
            "pod-shared",
            "Computer 有 pod_id 时 container_key 应为 pod_id"
        );
        adapter.insert("proj-B".to_string(), Arc::new(mk_proj("proj-B", "user-B"))).unwrap();

        // 两个 user 共享同一容器条目（refcount=2）。键为 container_name "computer-shared"。
        assert_eq!(adapter.containers.len(), 1, "两个 user 应共享同一容器条目");
        assert_eq!(
            adapter.containers.get("computer-shared").unwrap().ref_count(),
            2
        );

        // 任一 user 查询都能命中共享容器
        use shared_types::ContainerLookup;
        assert_eq!(
            adapter.find_by_user_id("user-A", &ServiceType::ComputerAgentRunner),
            Some("10.0.0.7".to_string())
        );
        assert_eq!(
            adapter.find_by_user_id("user-B", &ServiceType::ComputerAgentRunner),
            Some("10.0.0.7".to_string())
        );

        // 删除一个 user 的项目：容器仍存活（另一个 user 还在用）
        adapter.remove("proj-A");
        assert_eq!(adapter.containers.len(), 1, "容器应仍存活（user-B 还在用）");
        assert_eq!(
            adapter.containers.get("computer-shared").unwrap().ref_count(),
            1
        );

        // 删除最后一个：容器销毁
        adapter.remove("proj-B");
        assert_eq!(adapter.containers.len(), 0, "最后一个 user 移除后容器应销毁");
    }

    /// 回归测试：同 logical id 跨 ServiceType 不碰撞（container_name 键天然含 service_type 前缀）
    ///
    /// Computer user_id="6" 与 Web project_id="6" 共存。旧方案（裸 logical id 键）会撞键导致
    /// refcount 跨类型混算、查找互串；新方案（container_name 键）两条目独立。
    #[test]
    fn test_cross_service_type_no_key_collision() {
        use shared_types::ContainerLookup;
        let adapter = make_adapter();

        let mk = |name: &str, ip: &str| ContainerBasicInfo {
            container_id: format!("cid-{name}"),
            container_name: name.to_string(),
            container_ip: ip.to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{name}"),
        };

        // Computer 项目：user_id="6"，container_name 含 computer 前缀
        let mut comp = ProjectAndContainerInfo::from_parts(
            "proj-comp".to_string(),
            Some("6".to_string()),
            None,
            None,
            Some(mk("computer-agent-runner-6", "10.0.0.1")),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        comp.set_service_type(Some(ServiceType::ComputerAgentRunner));
        adapter
            .insert("proj-comp".to_string(), Arc::new(comp))
            .unwrap();

        // Web 项目：project_id="6"，container_name 含 web 前缀
        let mut web = ProjectAndContainerInfo::from_parts(
            "6".to_string(),
            None,
            None,
            None,
            Some(mk("web-agent-runner-6", "10.0.0.2")),
            ProjectExtendedFields {
                service_type: Some(ServiceType::WebAgentRunner),
                ..Default::default()
            },
        );
        web.set_service_type(Some(ServiceType::WebAgentRunner));
        adapter.insert("6".to_string(), Arc::new(web)).unwrap();

        // 两个独立容器条目（键不同：container_name 含 service_type 前缀）
        assert_eq!(
            adapter.containers.len(),
            2,
            "同 logical id=\"6\" 不同 service_type 应各自独立条目（不撞键）"
        );
        assert!(adapter.containers.contains_key("computer-agent-runner-6"));
        assert!(adapter.containers.contains_key("web-agent-runner-6"));

        // 查找互不串
        assert_eq!(
            adapter.find_by_user_id("6", &ServiceType::ComputerAgentRunner),
            Some("10.0.0.1".to_string()),
            "Computer 查找应命中 Computer 容器"
        );
        assert_eq!(
            adapter.find_by_project_id("6", &ServiceType::WebAgentRunner),
            Some("10.0.0.2".to_string()),
            "Web 查找应命中 Web 容器"
        );

        // RAII：删除 Computer 项目，仅销毁 Computer 容器，Web 容器不受影响
        adapter.remove("proj-comp");
        assert_eq!(adapter.containers.len(), 1, "Web 容器应仍存活");
        assert!(adapter.containers.contains_key("web-agent-runner-6"));
        assert!(!adapter.containers.contains_key("computer-agent-runner-6"));
    }

    /// 回归测试：跨重建稳定（container_name 确定性，重建不误增条目/不误动 refcount）
    ///
    /// 容器重建：container_id 变，但 container_name 不变（确定性命名）。
    /// 用 container_name 作键时，同 name 重复 insert → container_changed=false → 不触发 dec/inc。
    #[test]
    fn test_container_recreation_stability() {
        let adapter = make_adapter();

        let mk_proj = |cid: &str| {
            let container = ContainerBasicInfo {
                container_id: cid.to_string(),
                container_name: "computer-agent-runner-6".to_string(),
                container_ip: "10.0.0.1".to_string(),
                internal_port: 8086,
                external_port: 0,
                project_id: String::new(),
                status: "running".to_string(),
                created_at: Utc::now(),
                service_url: "http://c".to_string(),
            };
            let mut p = ProjectAndContainerInfo::from_parts(
                "proj-A".to_string(),
                Some("6".to_string()),
                None,
                None,
                Some(container),
                ProjectExtendedFields {
                    service_type: Some(ServiceType::ComputerAgentRunner),
                    ..Default::default()
                },
            );
            p.set_service_type(Some(ServiceType::ComputerAgentRunner));
            p
        };

        adapter
            .insert("proj-A".to_string(), Arc::new(mk_proj("cid-v1")))
            .unwrap();
        assert_eq!(adapter.containers.len(), 1);
        assert_eq!(
            adapter.containers.get("computer-agent-runner-6").unwrap().ref_count(),
            1
        );

        // 模拟容器重建：container_id 变（cid-v1→cid-v2），container_name 不变
        adapter
            .insert("proj-A".to_string(), Arc::new(mk_proj("cid-v2")))
            .unwrap();
        assert_eq!(
            adapter.containers.len(),
            1,
            "重建（同 container_name）不应新增容器条目"
        );
        assert_eq!(
            adapter.containers.get("computer-agent-runner-6").unwrap().ref_count(),
            1,
            "重建（同 container_name）refcount 应保持不变（不误触发 RAII）"
        );
        // 容器条目信息应刷新到新的 container_id（修复容器重建陈旧问题）
        assert_eq!(
            adapter
                .containers
                .get("computer-agent-runner-6")
                .unwrap()
                .info()
                .container_id,
            "cid-v2",
            "重建后容器条目应刷新为新 container_id，find_by_project_id 才能拿到新 ip"
        );
        // 旧 container_id 的反向索引应被清理（不累积陈旧条目）
        assert!(
            !adapter.container_id_to_key.contains_key("cid-v1"),
            "重建后旧 container_id 反向索引应被清理"
        );
        assert!(adapter.container_id_to_key.contains_key("cid-v2"));
    }

    #[test]
    fn test_index_pod_id_lookup() {
        // get_container_by_pod_id 通过索引 O(1) 查找（返回任一 project 的 container）；
        // find_projects_by_pod_id 全量遍历返回所有同 pod project（cleanup strategy 依赖此行为）
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-pod-1".to_string(),
            container_name: "pod-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        let mut info = ProjectAndContainerInfo::from_parts(
            "proj-1".to_string(),
            None,
            Some("pod-abc".to_string()),
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(ServiceType::WebAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // get_container_by_pod_id
        let found = adapter.get_container_by_pod_id("pod-abc");
        assert!(
            found.is_some(),
            "get_container_by_pod_id 应通过索引找到容器"
        );

        // find_projects_by_pod_id
        let projects = adapter.find_projects_by_pod_id("pod-abc");
        assert_eq!(projects.len(), 1, "find_projects_by_pod_id 应返回 1 个项目");
        assert_eq!(projects[0].project_id(), "proj-1");

        // 不存在的 pod_id
        assert!(adapter.get_container_by_pod_id("nonexistent").is_none());
        assert!(adapter.find_projects_by_pod_id("nonexistent").is_empty());
    }

    /// 多 project 共享同一 pod_id 时，find_projects_by_pod_id 必须返回全部。
    ///
    /// 回归测试：原实现用 pod_id_to_project_id 索引（insert 时覆盖），
    /// 只返回最后插入的 1 个，导致 cleanup strategy 误判"无活跃引用"误销毁容器。
    /// 现改为全量遍历，必须返回所有同 pod project。
    #[test]
    fn test_find_projects_by_pod_id_multiple_projects() {
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-shared".to_string(),
            container_name: "shared-pod".to_string(),
            container_ip: "10.0.0.5".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        // 两个 project 共享 pod_id="pod-shared"（RCoder 共享容器模式）
        for pid in ["proj-A", "proj-B"] {
            let mut info = ProjectAndContainerInfo::from_parts(
                pid.to_string(),
                None,
                Some("pod-shared".to_string()),
                None,
                Some(container.clone()),
                ProjectExtendedFields {
                    service_type: Some(ServiceType::WebAgentRunner),
                    ..Default::default()
                },
            );
            info.set_service_type(Some(ServiceType::WebAgentRunner));
            adapter.insert(pid.to_string(), Arc::new(info)).unwrap();
        }

        // 关键断言：find_projects_by_pod_id 必须返回 2 个 project（不是索引覆盖后的 1 个）
        let projects = adapter.find_projects_by_pod_id("pod-shared");
        assert_eq!(
            projects.len(),
            2,
            "find_projects_by_pod_id 必须返回所有同 pod project（全量遍历），不能只返回索引里的单个"
        );

        let project_ids: Vec<_> = projects.iter().map(|p| p.project_id()).collect();
        assert!(project_ids.contains(&"proj-A"));
        assert!(project_ids.contains(&"proj-B"));

        // get_container_by_pod_id 仍通过索引返回（任一 project 的 container，共享同一容器）
        let container_info = adapter.get_container_by_pod_id("pod-shared");
        assert!(
            container_info.is_some(),
            "get_container_by_pod_id 应能找到共享容器"
        );
    }

    #[test]
    fn test_index_cleanup_on_remove() {
        // remove 后 user_id/pod_id 索引应被清理
        // 注意：user_id 索引仅 ComputerAgentRunner 写入，故此处用 Computer 验证完整写入→清理路径
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-cleanup".to_string(),
            container_name: "cleanup-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        let mut info = ProjectAndContainerInfo::from_parts(
            "proj-1".to_string(),
            Some("user-cleanup".to_string()),
            Some("pod-cleanup".to_string()),
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // 索引存在
        assert!(adapter.user_id_to_project_ids.contains_key("user-cleanup"));
        assert!(adapter.pod_id_to_project_id.contains_key("pod-cleanup"));
        assert!(adapter.container_id_to_key.contains_key("cid-cleanup"));

        // remove 后索引应被清理
        adapter.remove("proj-1");
        assert!(
            !adapter.user_id_to_project_ids.contains_key("user-cleanup"),
            "user_id 索引应在 remove 后被清理"
        );
        assert!(
            !adapter.pod_id_to_project_id.contains_key("pod-cleanup"),
            "pod_id 索引应在 remove 后被清理"
        );
        // container_id_to_key 在 dec_container_ref 中清理（ref_count=0 时）
        assert!(
            !adapter.container_id_to_key.contains_key("cid-cleanup"),
            "container_id_to_key 索引应在 RAII 清理后被清理"
        );
    }

    #[test]
    fn test_index_cleanup_on_delete_container_with_projects() {
        // delete_container_with_projects 后所有索引应被清理
        let adapter = make_adapter();

        let container = ContainerBasicInfo {
            container_id: "cid-del".to_string(),
            container_name: "del-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        };

        let mut info = ProjectAndContainerInfo::from_parts(
            "proj-1".to_string(),
            Some("user-del".to_string()),
            None,
            None,
            Some(container.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // 索引存在
        assert!(adapter.container_id_to_key.contains_key("cid-del"));
        assert!(adapter.user_id_to_project_ids.contains_key("user-del"));

        // delete_container_with_projects 清理所有
        // 注意：remove 已通过 RAII 清理了容器（ref_count=0），所以 existed=false
        let (existed, count) = adapter.delete_container_with_projects("cid-del");
        assert!(!existed, "容器已被 RAII 清理（remove 时 ref_count 归零）");
        assert_eq!(count, 1, "应删除 1 个项目");

        // 索引应全部清理
        assert!(
            !adapter.container_id_to_key.contains_key("cid-del"),
            "container_id_to_key 索引应在 delete_container_with_projects 后被清理"
        );
        assert!(
            !adapter.user_id_to_project_ids.contains_key("user-del"),
            "user_id 索引应在 delete_container_with_projects 后被清理"
        );
        assert!(
            adapter.containers.is_empty(),
            "容器应在 delete_container_with_projects 后被清理"
        );
    }

    #[test]
    fn test_index_consistency_under_raii() {
        // 验证 RAII 清理后索引一致性：多个 project 共享容器
        let (adapter, _rx) = ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());

        let container = ContainerBasicInfo {
            container_id: "cid-shared".to_string(),
            container_name: "shared-container".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://shared".to_string(),
        };

        // 两个 project 共享同一容器（同一 user_id → container_key = user_id）
        let info1 = create_shared_project("proj-1", "user-shared", &container);
        let info2 = create_shared_project("proj-2", "user-shared", &container);

        adapter
            .insert("proj-1".to_string(), Arc::new(info1))
            .unwrap();
        adapter
            .insert("proj-2".to_string(), Arc::new(info2))
            .unwrap();

        assert_eq!(adapter.containers.len(), 1, "共享容器应只有 1 个条目");
        assert!(adapter.container_id_to_key.contains_key("cid-shared"));

        // 移除 proj-1：容器不销毁（ref_count > 0），索引保留
        adapter.remove("proj-1");
        assert_eq!(
            adapter.containers.len(),
            1,
            "容器应保留（还有 proj-2 引用）"
        );

        // 移除 proj-2：容器销毁（ref_count = 0），索引清理
        adapter.remove("proj-2");
        assert_eq!(
            adapter.containers.len(),
            0,
            "容器应在最后一个 project 移除后销毁"
        );
        assert!(
            !adapter.container_id_to_key.contains_key("cid-shared"),
            "container_id_to_key 索引应在 RAII 清理后被清理"
        );
        assert!(
            !adapter.user_id_to_project_ids.contains_key("user-shared"),
            "user_id 索引应在 remove 后被清理"
        );
    }

    /// 测试容器重建后 session_index 同步
    ///
    /// 场景：容器重建后，existing session 需要通过 add_session_to_project 同步到 session_index
    #[test]
    fn test_session_index_sync_after_container_rebuild() {
        let adapter = make_adapter();
        let project_id = "test-rebuild";
        let session_id = "session-before-rebuild";

        // 1. 创建项目并添加 session
        let info = Arc::new(create_test_info_with_container(project_id, "container-old"));
        adapter.insert(project_id.to_string(), info).unwrap();
        adapter.add_session_to_project(project_id, session_id);

        // 验证 session 可查
        assert!(adapter.get_by_session_id(session_id).is_some());

        // 2. 模拟容器重建：重新插入项目（不带 session）
        let new_info = Arc::new(create_test_info_with_container(project_id, "container-new"));
        adapter.insert(project_id.to_string(), new_info).unwrap();

        // 此时 session_index 中应该还有旧的 session（因为 insert 不清理 session_index）
        // 但 project 的 sessions 集合是空的
        let project = adapter.get(project_id).unwrap();
        assert_eq!(project.session_count(), 0, "新 project 的 sessions 应为空");

        // 3. 模拟 ensure_project_mapping_in_state 的修复逻辑：同步 session 到 session_index
        adapter.add_session_to_project(project_id, session_id);

        // 验证 session 现在可查
        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some(), "同步后 session 应可查");
        assert_eq!(by_session.unwrap().project_id(), project_id);

        // 验证 project 的 sessions 集合也包含了这个 session
        let project = adapter.get(project_id).unwrap();
        assert_eq!(project.session_count(), 1, "project 应包含 1 个 session");
    }

    /// 测试 get_by_session_id 验证 session 是否在 sessions 集合中
    ///
    /// 场景：session 被 clear_session_one 清除后，get_by_session_id 应返回 None
    #[test]
    fn test_get_by_session_id_validates_session_in_set() {
        let adapter = make_adapter();
        let project_id = "test-validate-session";
        let session_id = "session-to-validate";

        // 创建项目并添加 session
        let info = Arc::new(create_test_info(project_id));
        adapter.insert(project_id.to_string(), info).unwrap();
        adapter.add_session_to_project(project_id, session_id);

        // 验证 session 可查
        assert!(adapter.get_by_session_id(session_id).is_some());

        // 清除 session
        let cleared = adapter.clear_session_one(project_id, session_id);
        assert!(cleared);

        // 验证 session 不可查（关键断言）
        assert!(
            adapter.get_by_session_id(session_id).is_none(),
            "清除后的 session 不应被 get_by_session_id 返回"
        );

        // 验证 session_index 也被清理
        assert!(
            !adapter.session_index.contains_key(session_id),
            "session_index 应在 clear_session_one 后被清理"
        );
    }

    /// 测试 clear_session_one 的顺序：先清理 session_index，再清理 projects
    ///
    /// 场景：验证 clear_session_one 后，session_index 和 projects 都被正确清理
    #[test]
    fn test_clear_session_one_order() {
        let adapter = make_adapter();
        let project_id = "test-clear-order";
        let session_id = "session-clear-order";

        // 创建项目并添加 session
        let info = Arc::new(create_test_info(project_id));
        adapter.insert(project_id.to_string(), info).unwrap();
        adapter.add_session_to_project(project_id, session_id);

        // 验证 session_index 和 projects 都有这个 session
        assert!(adapter.session_index.contains_key(session_id));
        let project = adapter.get(project_id).unwrap();
        assert!(project.sessions().contains(session_id));

        // 清除 session
        let cleared = adapter.clear_session_one(project_id, session_id);
        assert!(cleared);

        // 验证 session_index 被清理
        assert!(
            !adapter.session_index.contains_key(session_id),
            "session_index 应被清理"
        );

        // 验证 projects 中的 sessions 集合也被清理
        let project = adapter.get(project_id).unwrap();
        assert!(
            !project.sessions().contains(session_id),
            "projects 中的 sessions 集合应被清理"
        );
    }

    /// 测试容器重建场景：insert 后 add_session_to_project 的完整流程
    ///
    /// 模拟 computer_chat_handler 中 ensure_project_mapping_in_state 的逻辑
    #[test]
    fn test_container_rebuild_with_session_migration() {
        let adapter = make_adapter();
        let project_id = "test-rebuild-migration";
        let user_id = "user-123";

        // 1. 初始状态：创建项目，添加 2 个 session
        let mut info = create_test_info_with_container(project_id, "container-v1");
        info.set_user_id(Some(user_id.to_string()));
        adapter
            .insert(project_id.to_string(), Arc::new(info))
            .unwrap();
        adapter.add_session_to_project(project_id, "session-1");
        adapter.add_session_to_project(project_id, "session-2");

        // 验证初始状态
        assert_eq!(adapter.get(project_id).unwrap().session_count(), 2);
        assert!(adapter.get_by_session_id("session-1").is_some());
        assert!(adapter.get_by_session_id("session-2").is_some());

        // 2. 模拟容器重建：获取 existing sessions
        let existing_sessions: Vec<String> = adapter
            .get(project_id)
            .map(|p| p.sessions().into_iter().collect())
            .unwrap_or_default();
        assert_eq!(existing_sessions.len(), 2);

        // 3. 插入新的 project（模拟容器重建）
        let mut new_info = create_test_info_with_container(project_id, "container-v2");
        new_info.set_user_id(Some(user_id.to_string()));
        adapter
            .insert(project_id.to_string(), Arc::new(new_info))
            .unwrap();

        // 4. 同步现有 session 到 session_index（修复逻辑）
        for sid in &existing_sessions {
            adapter.add_session_to_project(project_id, sid);
        }

        // 5. 验证所有 session 都可查
        assert!(
            adapter.get_by_session_id("session-1").is_some(),
            "session-1 应可查"
        );
        assert!(
            adapter.get_by_session_id("session-2").is_some(),
            "session-2 应可查"
        );

        // 6. 验证 project 的 sessions 集合也正确
        let project = adapter.get(project_id).unwrap();
        assert_eq!(project.session_count(), 2, "project 应包含 2 个 session");

        // 7. 添加新 session（新请求）
        adapter.add_session_to_project(project_id, "session-3");
        assert!(
            adapter.get_by_session_id("session-3").is_some(),
            "新 session-3 应可查"
        );

        let project = adapter.get(project_id).unwrap();
        assert_eq!(project.session_count(), 3, "project 应包含 3 个 session");
    }
}

// ========== ContainerLookup trait 实现 ==========

impl shared_types::ContainerLookup for ProjectAdapter {
    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    ///
    /// user_id 是 1:N（一个 user 可有多个 project），无法用单值索引精确反查。
    /// 此处全量扫描（`find_projects_by_user_id`，已按 `service_type` 过滤）取任一
    /// 匹配 project 的容器 IP——同 user 的 Computer 项目共享同一容器，任取一个即可。
    /// O(N)，N 为 project 总数（通常很小）。
    fn find_by_user_id(&self, user_id: &str, service_type: &shared_types::ServiceType) -> Option<String> {
        self.find_projects_by_user_id(user_id, service_type)
            .into_iter()
            .next()
            .and_then(|p| p.container().map(|c| c.container_ip.clone()))
    }

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    ///
    /// 通过 project_to_container 索引找到 container_key，
    /// 然后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_project_id(&self, project_id: &str, service_type: &shared_types::ServiceType) -> Option<String> {
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
        Some(entry.info().container_ip.clone())
    }

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    ///
    /// 通过 pod_id_to_project_id 索引找到 project_id，
    /// 然后通过 project_to_container 索引找到 container_key，
    /// 最后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None，
    /// 避免同一 pod_id 下跨 ServiceType 容器互相串用。
    fn find_by_pod_id(&self, pod_id: &str, service_type: &shared_types::ServiceType) -> Option<String> {
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
        Some(entry.info().container_ip.clone())
    }
}

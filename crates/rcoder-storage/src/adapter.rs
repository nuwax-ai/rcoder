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
//! ## 子模块（同类型 extension-impl 拆分）
//!
//! - `container_ops`: 容器条目 CRUD/反查/引用计数
//! - `lookup`: Pingora 查询面 + ContainerLookup 实现
//! - `session_ops`: session 映射操作
//! - `store_impl`: ProjectStore 契约实现

mod container_ops;
mod lookup;
mod session_ops;
mod store_impl;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::DashSet;
use dashmap::mapref::entry::Entry;
use shared_types::ProjectAndContainerInfo;
use tracing::{debug, info};

use lockmap::LockMap;
use shared_types::CleanupRequest;
use shared_types::ContainerEntry;
use shared_types::StorageStats;

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
    ///
    /// 字段 `pub(super)`：供 `adapter_session_ops.rs`（同类型 extension-impl 拆分）访问。
    pub(super) session_index: DashMap<String, (String, String)>,
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
    /// per-project 串行锁：序列化同一 project_id 的 insert/remove，
    /// 消除跨 DashMap（projects ↔ containers）的 TOCTOU 竞态。
    /// `Arc<LockMap>` 保证 clone 后共享同一锁实例。
    project_locks: Arc<LockMap<String, ()>>,
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
        let (tx, rx) = tokio::sync::mpsc::channel(shared_types::CLEANUP_CHANNEL_CAPACITY);
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
            project_locks: Arc::new(LockMap::new()),
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
        // per-project 锁（lockmap）：序列化同一 project_id 的并发 insert/remove，
        // 消除 projects ↔ containers 跨 DashMap 的 TOCTOU 竞态（ref_count 泄漏根因）。
        // lockmap 的 entry_by_ref 阻塞获取 per-key 排他锁，guard drop 自动释放 + 自动清理。
        let _project_guard = self.project_locks.entry_by_ref(&project_id);
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
        // per-project 锁（lockmap）：与 insert 共享，序列化同 project 并发操作。
        let _project_guard = self.project_locks.entry_by_ref(project_id);

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
pub(crate) fn code_to_agent_status(code: i32, _name: &str) -> shared_types::AgentStatus {
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
pub(crate) fn container_entry_key(info: &ProjectAndContainerInfo) -> String {
    match info.container_info() {
        Some(c) => c.container_name.clone(),
        None => info.container_key().to_string(),
    }
}

#[cfg(test)]
mod tests;

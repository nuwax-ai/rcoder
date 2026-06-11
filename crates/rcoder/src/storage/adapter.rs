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
}

impl ProjectAdapter {
    /// 创建新的项目适配器
    ///
    /// 返回 (adapter, cleanup_receiver)。
    /// cleanup_receiver 需要传给 ResourceReaper 以处理容器销毁。
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let adapter = Self {
            projects: DashMap::new(),
            containers: DashMap::new(),
            session_index: DashMap::new(),
            project_to_container: DashMap::new(),
            cleanup_tx: tx,
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
    pub fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) -> anyhow::Result<()> {
        let container_key = info.container_key().to_string();

        // 读取旧 container_key（view: 读后立即释放锁）
        let old_ck = self
            .projects
            .view(&project_id, |_, v| v.container_key().to_string());

        // 容器是否变更
        let container_changed = match &old_ck {
            Some(old) => *old != container_key,
            None => true, // 新 project，需要 inc_ref
        };

        // 旧容器引用 -1（容器变更时）
        if let Some(old) = old_ck
            && container_changed
        {
            self.dec_container_ref(&old);
        }

        // 新容器引用 +1（仅容器变更或新 project 时）— entry API 原子操作
        if container_changed
            && let Some(container) = info.container()
        {
            let st = match info.service_type() {
                Some(st) => st,
                None => {
                    tracing::error!(
                        "[STORAGE] service_type is None, cannot insert project: project_id={}, container_key={}",
                        project_id, container_key
                    );
                    return Err(anyhow::anyhow!(
                        "service_type is required for project insert: project_id={}",
                        project_id
                    ));
                }
            };
            match self.containers.entry(container_key.clone()) {
                Entry::Occupied(e) => {
                    e.get().inc_ref();
                }
                Entry::Vacant(e) => {
                    e.insert(Arc::new(ContainerEntry::new(container.clone(), st)));
                }
            }
        }

        // 写入主存储和索引
        self.project_to_container
            .insert(project_id.clone(), container_key);
        self.projects.insert(project_id, info);
        Ok(())
    }

    /// 删除项目（RAII 核心）
    ///
    /// 自动清理 session 索引和容器引用计数。
    /// 容器引用归零时触发异步物理销毁。
    pub fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 1. 先从主存储移除，获取 info 所有权（避免后续从 map 读取时被并发修改）
        let (_, info) = self.projects.remove(project_id)?;

        // 2. 从已获取的 info 中读取 session_id（无需再锁 projects map）
        if let Some(sid) = info.session_id() {
            self.session_index.remove(sid);
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

        debug!("[STORAGE] removed project: {}", project_id);
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
    pub fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let pid = self
            .session_index
            .view(session_id, |_, v| v.1.clone())?;

        match self.projects.view(&pid, |_, v| v.clone()) {
            Some(info) => Some(info),
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

    /// 插入项目并设置 session 映射（原子操作，消除 CAS 竞态）
    ///
    /// # Errors
    /// 如果 `service_type` 未设置，透传 `insert` 的错误。
    pub fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // 保存旧 session_id（view: 读后立即释放锁）
        let old_sid = self
            .projects
            .view(&project_id, |_, v| v.session_id().map(String::from))
            .flatten();

        // 执行 insert（维护容器引用计数）
        self.insert(project_id.clone(), info)?;

        // 清理旧 session 索引
        if let Some(ref old) = old_sid
            && session_id.is_none_or(|s| s != old)
        {
            self.session_index.remove(old);
        }

        // 写入新 session 索引 + 更新 project info
        if let Some(sid) = session_id {
            let ck = self
                .project_to_container
                .view(&project_id, |_, v| v.clone())
                .unwrap_or_default();
            self.session_index
                .insert(sid.to_string(), (ck, project_id.clone()));

            // entry API: 精确锁定单条记录修改 session_id
            if let Entry::Occupied(mut e) = self.projects.entry(project_id) {
                let info = Arc::make_mut(e.get_mut());
                info.set_session_id(Some(sid.to_string()));
            }
        }
        Ok(())
    }
    pub fn update_session(&self, project_id: &str, session_id: &str) {
        self.write_session_index(project_id, session_id);

        // entry API: 精确锁定单条记录修改 session_id
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = Arc::make_mut(e.get_mut());
            info.set_session_id(Some(session_id.to_string()));
        }

        // 更新容器活跃时间（view: 读 ck 后立即释放锁）
        if let Some(ck) = self
            .project_to_container
            .view(project_id, |_, v| v.clone())
        {
            self.containers.view(&ck, |_, ce| ce.update_activity());
        }
    }

    /// 原子更新 session（CAS 语义）
    #[allow(dead_code)]
    pub fn update_session_atomic(
        &self,
        project_id: &str,
        new_session_id: &str,
        expected_current_session_id: Option<&str>,
    ) -> bool {
        let mut updated = false;

        // entry API: CAS 在写锁范围内完成
        if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = e.get();
            let current = info.session_id().map(String::from);
            let matches = match (expected_current_session_id, &current) {
                (Some(expected), Some(cur)) => expected == cur,
                (None, None) => true,
                _ => false,
            };
            if matches {
                let info = Arc::make_mut(e.get_mut());
                info.set_session_id(Some(new_session_id.to_string()));
                updated = true;
            }
        }

        if updated {
            self.write_session_index(project_id, new_session_id);
        }
        updated
    }

    /// 清除 session
    pub fn clear_session(&self, project_id: &str) {
        // entry API: 读取旧值 + 清除在同一写锁内完成
        let old_sid = if let Entry::Occupied(mut e) = self.projects.entry(project_id.to_string()) {
            let info = e.get();
            let old = info.session_id().map(String::from);
            let info = Arc::make_mut(e.get_mut());
            info.clear_session_id();
            old
        } else {
            None
        };

        if let Some(sid) = old_sid {
            self.session_index.remove(&sid);
            debug!(
                "[STORAGE] cleared session: project_id={}, sid={}",
                project_id, sid
            );
        }
    }

    // ========== Session → Container ==========

    /// 通过 session_id 获取容器名称（view: 两次锁获取均立即释放）
    pub fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        let ck = self.session_index.view(session_id, |_, v| v.0.clone())?;
        self.containers.view(&ck, |_, ce| ce.info().container_name.clone())
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
            container_key = Some(info.container_key().to_string());
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

        // view() 获取 Arc（DashMap 读锁立即释放），通过 RwLock 更新字段
        let existing = self.containers.view(&container.container_id, |_, ce| ce.clone());

        match existing {
            Some(ce) => {
                ce.update(container.clone(), st);
            }
            None => {
                // 使用 entry() API 原子插入，避免 view()+insert() 的 TOCTOU 竞态
                match self.containers.entry(container.container_id.clone()) {
                    Entry::Occupied(e) => {
                        // 并发插入：另一个线程已插入，直接更新
                        e.get().update(container.clone(), st);
                    }
                    Entry::Vacant(e) => {
                        e.insert(Arc::new(ContainerEntry::with_ref_count(container.clone(), st, 0)));
                    }
                }
            }
        }
        Ok(())
    }

    /// 获取容器信息（按 container_id 查找）
    pub fn get_container(&self, container_id: &str) -> Option<ContainerBasicInfo> {
        self.containers
            .iter()
            .find(|e| e.value().info().container_id == container_id)
            .map(|e| e.value().info())
    }

    /// 删除容器及其关联的所有项目（RAII 触发物理销毁）
    ///
    /// 返回 (容器是否存在, 删除的项目数)
    pub fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize) {
        // 收集所有关联此容器的 project_id
        let project_ids: Vec<String> = self
            .projects
            .iter()
            .filter(|e| {
                e.container()
                    .map(|c| c.container_id == container_id)
                    .unwrap_or(false)
            })
            .map(|e| e.key().clone())
            .collect();

        let count = project_ids.len();

        // 逐个移除（每个 remove 都会 dec_ref）
        for pid in &project_ids {
            self.remove(pid);
        }

        // 如果容器条目还存在（没有 project 触发清理），用 entry 原子移除
        let ck_to_remove: Option<String> = self
            .containers
            .iter()
            .find(|e| e.value().info().container_id == container_id)
            .map(|e| e.key().clone());

        let container_existed = ck_to_remove.is_some();
        if let Some(ck) = ck_to_remove
            && let Entry::Occupied(e) = self.containers.entry(ck)
        {
            let (container_key, entry) = e.remove_entry();
            let info = entry.info();
            let _ = self.cleanup_tx.send(CleanupRequest {
                identifier: container_key,
                container_name: info.container_name,
                service_type: entry.service_type(),
                container_ip: info.container_ip,
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
        self.containers
            .iter()
            .map(|e| e.value().info())
            .collect()
    }

    /// 根据 container_id 获取关联的项目列表
    pub fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.projects
            .iter()
            .filter(|e| {
                e.container()
                    .map(|c| c.container_id == container_id)
                    .unwrap_or(false)
            })
            .map(|e| e.value().clone())
            .collect()
    }

    // ========== ComputerAgentRunner 模式 ==========

    /// 通过 user_id 获取容器信息
    pub fn get_container_by_user_id(&self, user_id: &str) -> Option<ContainerBasicInfo> {
        self.projects.iter().find_map(|e| {
            if e.value().user_id() == Some(user_id) {
                e.value().container().cloned()
            } else {
                None
            }
        })
    }

    /// 通过 pod_id 获取容器信息
    pub fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        self.projects.iter().find_map(|e| {
            if e.value().pod_id() == Some(pod_id) {
                e.value().container().cloned()
            } else {
                None
            }
        })
    }

    /// 通过 user_id 查找所有项目
    pub fn find_projects_by_user_id(
        &self,
        user_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.projects
            .iter()
            .filter(|e| e.value().user_id() == Some(user_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// 通过 pod_id 查找所有项目
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

    /// 写入 session 双向索引
    ///
    /// 使用 view() 读旧值（锁立即释放），再单独写入，避免 entry 持锁期间跨 map 操作。
    fn write_session_index(&self, project_id: &str, session_id: &str) {
        // 1. 读旧 session_id（view: 锁立即释放）
        let old_sid = self
            .projects
            .view(project_id, |_, v| v.session_id().map(String::from))
            .flatten();

        // 2. 清除旧正向映射
        if let Some(ref old) = old_sid
            && old != session_id
        {
            self.session_index.remove(old);
        }

        // 3. 写入新映射（view: 读 ck 后立即释放锁）
        let ck = self
            .project_to_container
            .view(project_id, |_, v| v.clone())
            .unwrap_or_default();

        self.session_index
            .insert(session_id.to_string(), (ck, project_id.to_string()));
    }

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
            let (ck, entry) = entry.remove_entry();
            let info = entry.info();
            info!(
                "[STORAGE] RAII: container refcount=0, sending cleanup for {}",
                info.container_name
            );
            let _ = self.cleanup_tx.send(CleanupRequest {
                identifier: ck,
                container_name: info.container_name,
                service_type: entry.service_type(),
                container_ip: info.container_ip,
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::ProjectExtendedFields;

    fn create_test_info(project_id: &str) -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new(project_id.to_string());
        info.set_service_type(Some(ServiceType::RCoder));
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
        let (adapter, _) = ProjectAdapter::new();
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

        adapter.update_session(project_id, session_id);

        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some());
        assert_eq!(by_session.unwrap().project_id(), project_id);

        let container_name = adapter.get_container_name_by_session(session_id);
        assert_eq!(container_name, Some("container-1".to_string()));
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

    #[test]
    fn test_session_rotation() {
        let adapter = make_adapter();
        let project_id = "test-rotation";
        let info = Arc::new(create_test_info(project_id));

        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some("session-1"))
            .unwrap();
        assert!(adapter.get_by_session_id("session-1").is_some());

        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some("session-2"))
            .unwrap();
        assert!(adapter.get_by_session_id("session-2").is_some());
        assert!(adapter.get_by_session_id("session-1").is_none());
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

        adapter
            .insert("proj-A".to_string(), info.clone())
            .unwrap();
        assert_eq!(adapter.containers.len(), 1);

        adapter
            .insert("proj-A".to_string(), info.clone())
            .unwrap();
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

        // container_key 对于 RCoder 是 pod_id 或 project_id
        // 这里使用 project_id 作为 container_id，确保 save_container 和 insert 使用相同的 key
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
            .save_container(&container, Some(ServiceType::RCoder))
            .unwrap();
        assert_eq!(adapter.containers.len(), 1);

        // 通过 project insert 关联容器（ref_count 0→1）
        let mut info = create_test_info("proj-1");
        info.set_container(Some(container.clone()));
        adapter
            .insert("proj-1".to_string(), Arc::new(info))
            .unwrap();

        // 验证 ref_count = 1
        let ce = adapter.containers.get("proj-1").unwrap();
        assert_eq!(ce.value().ref_count(), 1);

        // 第二次 save：更新已有条目，ref_count 应保持不变
        let mut updated_container = container.clone();
        updated_container.container_name = "updated-name".to_string();
        adapter
            .save_container(&updated_container, Some(ServiceType::ComputerAgentRunner))
            .unwrap();

        let ce = adapter.containers.get("proj-1").unwrap();
        assert_eq!(ce.value().ref_count(), 1, "save_container 更新不应改变 ref_count");
        assert_eq!(ce.value().info().container_name, "updated-name");
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
        let (adapter, rx) = ProjectAdapter::new();
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
        let (adapter, _rx) = ProjectAdapter::new();
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
        let (adapter, rx) = ProjectAdapter::new();
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
        assert_eq!(adapter.containers.len(), 0, "container should be cleaned up");

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
        let (adapter, _rx) = ProjectAdapter::new();
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
                    adapter.update_session(pid, &sid);
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
        let (adapter, _rx) = ProjectAdapter::new();
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
                    let _ = adapter.insert_with_session(
                        pid.to_string(),
                        info,
                        Some(&sid),
                    );
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
        let (adapter, _rx) = ProjectAdapter::new();
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
                    assert!(
                        result.is_none(),
                        "removing nonexistent should return None"
                    );
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
        let (adapter, _rx) = ProjectAdapter::new();
        let adapter = Arc::new(adapter);

        for i in 0..10 {
            let pid = format!("preload-{}", i);
            let info =
                Arc::new(create_test_info_with_container(&pid, &format!("c-pre-{}", i)));
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
                    adapter.update_session(&pid, &sid);
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
        let (adapter, rx) = ProjectAdapter::new();
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
        assert_eq!(req.service_type, ServiceType::RCoder);
    }

    #[test]
    fn test_shared_container_ref_count_no_leak_under_reinsert() {
        let (adapter, rx) = ProjectAdapter::new();
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
        assert_eq!(cleanups.len(), 5, "5 rounds should produce 5 cleanup requests");
    }
}

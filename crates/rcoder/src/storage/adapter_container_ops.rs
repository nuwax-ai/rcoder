//! 容器记录 CRUD（从 adapter.rs 拆出，extension-impl）。
//!
//! 容器条目的保存/删除/查询、按 user_id/pod_id/container_id 反查容器与项目、
//! 闲置容器扫描与引用计数（RAII）。项目/会话映射方法仍留在 adapter.rs。

use std::sync::Arc;

use chrono::Utc;
use dashmap::mapref::entry::Entry;
use shared_types::{ContainerBasicInfo, ContainerEntry, ProjectAndContainerInfo, ServiceType};
use tracing::info;

use super::adapter::ProjectAdapter;
use super::resource_reaper::CleanupRequest;
use super::types::IdleContainerInfo;

impl ProjectAdapter {
    // ========== 容器操作 ==========

    /// 保存容器信息（更新或创建）
    ///
    /// 通过 view() 获取 `Arc<ContainerEntry>`，利用内部可变性（RwLock）更新。
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
            if let Err(e) = self.cleanup_tx.try_send(CleanupRequest {
                identifier: entry.logical_id().to_string(),
                container_name: info.container_name,
                service_type: entry.service_type(),
                container_ip: info.container_ip,
                namespace: self.namespace.clone(),
                cluster_domain: self.cluster_domain.clone(),
                project_ids,
                retry_count: 0,
            }) {
                tracing::error!(
                    "[STORAGE] cleanup channel try_send failed (full or ResourceReaper down?), container leak risk: identifier={}, {}",
                    entry.logical_id(),
                    e
                );
            }
        }

        (container_existed, count)
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
        let p = self
            .find_projects_by_user_id(user_id, service_type)
            .into_iter()
            .next()?;
        // 统一走 containers[name] 权威源（service_type 已由 find_projects_by_user_id 过滤）
        self.container_info_by_project(p.project_id())
    }

    /// 通过 pod_id 获取容器信息（O(1) 索引查找）
    pub fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        let project_id = self.pod_id_to_project_id.get(pod_id)?.value().clone();
        self.container_info_by_project(&project_id)
    }

    /// 通过 project_id 解析其唯一容器的信息（统一权威源 `containers[name]`）。
    ///
    /// project_id 与容器是 1:1（一个 project_id 永远只对应一个容器），故
    /// `project_to_container[project_id] → containers[name]` 对有容器的 project 永远成立。
    /// 所有容器信息查找（find_by_* / get_container_by_*）统一走此路径，
    /// `projects[pid].container()` 仅作 project 自身视图快照，不作为查找依据。
    fn container_info_by_project(&self, project_id: &str) -> Option<ContainerBasicInfo> {
        let name = self.project_to_container.get(project_id)?.value().clone();
        self.containers.get(&name).map(|e| e.info())
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
                        p.container_info()
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

    // ========== 内部方法 ==========

    /// 减少容器引用计数，归零时触发 RAII 清理
    ///
    /// 使用 entry() API 实现 dec_ref + remove 的原子操作，
    /// 消除 view() + remove() 之间的 TOCTOU 竞态。
    ///
    /// 注：`pub(super)` 供 adapter.rs 的 `insert` / `remove`（项目 CRUD）跨文件调用。
    pub(super) fn dec_container_ref(&self, container_key: &str) {
        let entry = match self.containers.entry(container_key.to_string()) {
            Entry::Occupied(e) => e,
            Entry::Vacant(_) => return,
        };

        // dec_ref 在 entry 写锁范围内，与后续 remove_entry 原子
        let remaining = entry.get().dec_ref();
        if remaining == 0 {
            // refcount=0（无 project 关联）: 清理 map 条目,但不发 cleanup_tx 物理销毁。
            // 物理销毁由调用方负责: agent_stop 已 stop_container_by_identifier;
            // cleaner idle 标记 Idle 保留容器(不删 project),long_idle 才销毁。
            // 历史:refcount=0 立即 cleanup_tx 会绕过 cleaner idle 回收,导致容器在保护期内被误删。
            let (_ck, entry) = entry.remove_entry();
            let info = entry.info();
            self.container_id_to_key.remove(&info.container_id);
            info!(
                "[STORAGE] container refcount=0,清理 map 条目(不物理销毁): {}",
                info.container_name
            );
        }
    }
}

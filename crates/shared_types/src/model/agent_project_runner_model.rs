use chrono::{DateTime, Utc};
use im::HashSet as ImHashSet;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{AgentStatus, ModelProviderConfig};
use crate::{ContainerEntry, ServiceType};

/// 容器基本信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerBasicInfo {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 容器IP地址
    pub container_ip: String,
    /// 内部端口
    pub internal_port: u16,
    /// 外部端口
    pub external_port: u16,
    /// 项目ID
    pub project_id: String,
    /// 容器状态
    pub status: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 服务URL
    pub service_url: String,
}

/// 项目核心状态 - 包含频繁变更的小字段
///
/// 这些字段在每次请求中都会被更新，需要高效访问和修改
#[derive(Debug, Clone)]
pub struct ProjectCoreState {
    /// 项目ID
    pub project_id: String,
    /// 用户ID（ComputerAgentRunner 模式专用）
    /// - RCoder 模式：None（使用 project_id 作为容器唯一标识）
    /// - ComputerAgentRunner 模式：Some(user_id)（使用 user_id 作为容器唯一标识）
    pub user_id: Option<String>,
    /// Pod ID（共享容器模式）
    /// - RCoder 模式：当 pod_id 有值时，多个项目共享同一个容器
    /// - ComputerAgentRunner 模式：通常为 None（使用 user_id 共享）
    pub pod_id: Option<String>,
    /// 租户 ID（多租户隔离）：共享容器（tenant/space 隔离）下项目所属租户。
    /// 用于按 project_id 反查项目归属（如终端 cwd 三级路径解析）。None=非共享/未知。
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离）：共享容器下项目所属空间（tenant 下的二级分组）。
    pub space_id: Option<String>,
    /// 隔离类型（tenant/space/project）。仅作记录与日志；cwd 路径决策依据 tenant/space 的有无。
    pub isolation_type: Option<String>,
    /// 该 project 关联的所有 session_id 集合
    ///
    /// 一个 project 可以同时有多个活跃 session（多窗口/多标签场景）。
    /// 使用 `Arc<im::HashSet>` 实现结构共享：每次更新返回新的 Arc，
    /// 未变更部分零拷贝。读快照（cheap clone）用于无锁读路径。
    pub sessions: Arc<ImHashSet<String>>,
    /// 最近一次添加的 session_id（兼容旧 `session_id()` 单值读路径）
    ///
    /// 维护成本：add_session 时更新为 sid；remove_session 时若删的正是 latest，
    /// 退化到任意剩余 session（iter 顺序无意义但稳定）或 None。
    pub latest_session: Option<String>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
}

impl ProjectCoreState {
    pub fn new(project_id: String) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            user_id: None,
            pod_id: None,
            tenant_id: None,
            space_id: None,
            isolation_type: None,
            sessions: Arc::new(ImHashSet::new()),
            latest_session: None,
            last_activity: now,
            created_at: now,
        }
    }

    /// 创建带 user_id 的核心状态（ComputerAgentRunner 模式）
    #[allow(dead_code)]
    pub fn new_with_user_id(project_id: String, user_id: String) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            user_id: Some(user_id),
            pod_id: None,
            tenant_id: None,
            space_id: None,
            isolation_type: None,
            sessions: Arc::new(ImHashSet::new()),
            latest_session: None,
            last_activity: now,
            created_at: now,
        }
    }

    /// 添加 session 到集合（C2 修复核心）
    ///
    /// - 不清除其他 session（一个 project 允许多个活跃 session 并存）
    /// - 更新 `latest_session` 为本次添加的 sid
    /// - 更新 `last_activity`
    /// - `Arc<im::HashSet>` 的写时复制：仅 O(log n) 增量分配，未变更节点共享
    pub fn add_session(&mut self, session_id: impl Into<String>) {
        let sid = session_id.into();
        // Arc::make_mut 在 ref_count==1 时原地修改，否则克隆内部 HashSet
        let set = Arc::make_mut(&mut self.sessions);
        set.insert(sid.clone());
        self.latest_session = Some(sid);
        self.last_activity = Utc::now();
    }

    /// 恢复专用（boot 加载 / sync 重建 / 回源 hydrate）：与 [`Self::add_session`]
    /// 的集合语义一致，但**不触碰 `last_activity`**——活跃时间以持久化行为准，
    /// 恢复动作本身不代表用户活跃（否则重启/回源即把全部 idle 计时归零，
    /// 闲置回收系统性推迟，且"恢复时刻"会经后续 upsert 写回 PG 污染活跃历史）。
    pub fn restore_session(&mut self, session_id: impl Into<String>) {
        let sid = session_id.into();
        let set = Arc::make_mut(&mut self.sessions);
        set.insert(sid.clone());
        self.latest_session = Some(sid);
    }

    /// 移除指定 session
    ///
    /// 返回 true 表示该 session 之前存在并已被移除。
    /// 若移除的是 `latest_session`，从剩余 sessions 中任选一个作为新 latest
    ///（im::HashSet 迭代顺序稳定但无意义，这里只是为了不返回 None 误误导读路径）。
    pub fn remove_session(&mut self, session_id: &str) -> bool {
        let set = Arc::make_mut(&mut self.sessions);
        let removed = set.remove(session_id).is_some();
        if removed && self.latest_session.as_deref() == Some(session_id) {
            self.latest_session = set.iter().next().cloned();
        }
        if removed {
            self.last_activity = Utc::now();
        }
        removed
    }

    /// 返回 sessions 的廉价快照（im::HashSet clone 是 O(1) Arc bump）
    pub fn sessions(&self) -> ImHashSet<String> {
        (*self.sessions).clone()
    }

    /// 当前活跃 session 数量
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// 清空所有 session
    pub fn clear_all_sessions(&mut self) {
        let set = Arc::make_mut(&mut self.sessions);
        set.clear();
        self.latest_session = None;
        self.last_activity = Utc::now();
    }

    /// 更新活动时间 - 高频操作
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }
}

/// 项目扩展状态 - 包含较少变更的大字段
///
/// 用于 `from_parts` 构造的可选字段参数
#[derive(Debug, Clone, Default)]
pub struct ProjectExtendedFields {
    pub request_id: Option<String>,
    pub service_type: Option<ServiceType>,
    pub last_activity: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    /// 租户 ID（共享容器隔离）：用于 from_parts 构造时回填 ProjectCoreState.tenant_id
    pub tenant_id: Option<String>,
    /// 空间 ID（共享容器隔离）
    pub space_id: Option<String>,
    /// 隔离类型（tenant/space/project）
    pub isolation_type: Option<String>,
}

/// 这些字段相对稳定，不需要频繁更新
#[derive(Debug, Clone)]
pub struct ProjectExtendedState {
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// container 容器信息（Arc 共享：与 `ProjectAdapter.containers[name]` 同一实例），
    /// 一个 project_id 只能对应最多1个容器。
    pub container: Option<Arc<ContainerEntry>>,
    /// 当前活跃的请求ID，用于标识用户请求
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: Option<AgentStatus>,
    /// 服务类型
    pub service_type: Option<ServiceType>,
}

impl ProjectExtendedState {
    pub fn new() -> Self {
        Self {
            model_provider: None,
            container: None,
            request_id: None,
            status: None,
            service_type: None,
        }
    }

    /// 批量更新扩展状态（container 接收已包装的 Arc<ContainerEntry>）
    pub fn update_from_request(
        &mut self,
        container: Option<Arc<ContainerEntry>>,
        model_provider: Option<ModelProviderConfig>,
        request_id: Option<String>,
        service_type: Option<ServiceType>,
    ) {
        self.container = container;
        self.model_provider = model_provider;
        self.request_id = request_id;
        if let Some(st) = service_type {
            self.service_type = Some(st);
        }
    }
}

/// 项目状态包装器 - 使用 Arc 实现高效的共享和写时复制
///
/// 这个结构优化了克隆性能，避免不必要的数据复制
#[derive(Debug, Clone)]
pub struct ProjectState {
    /// 核心状态 - 使用 Arc 实现高效共享
    pub core: Arc<ProjectCoreState>,
    /// 扩展状态 - 使用 Arc 实现写时复制
    pub extended: Arc<ProjectExtendedState>,
}

impl ProjectState {
    pub fn new(project_id: String) -> Self {
        Self {
            core: Arc::new(ProjectCoreState::new(project_id)),
            extended: Arc::new(ProjectExtendedState::new()),
        }
    }

    /// 高效更新核心状态 - 使用 Arc::make_mut 避免不必要的克隆
    pub fn update_core<F>(&mut self, updater: F)
    where
        F: FnOnce(&mut ProjectCoreState),
    {
        // 使用 Arc::make_mut 实现写时复制
        let core = Arc::make_mut(&mut self.core);
        updater(core);
    }

    /// 高效更新扩展状态 - 使用 Arc::make_mut 避免不必要的克隆
    pub fn update_extended<F>(&mut self, updater: F)
    where
        F: FnOnce(&mut ProjectExtendedState),
    {
        // 使用 Arc::make_mut 实现写时复制
        let extended = Arc::make_mut(&mut self.extended);
        updater(extended);
    }

    /// 获取项目ID的便捷方法
    pub fn project_id(&self) -> &str {
        &self.core.project_id
    }

    /// 获取最新 session_id 的便捷方法（多 session 模型下返回 latest_session）
    ///
    /// 兼容历史单 session 读路径。需要全量 sessions 请用 `sessions()`。
    pub fn session_id(&self) -> Option<&str> {
        self.core.latest_session.as_deref()
    }

    /// 获取 sessions 集合的廉价快照
    pub fn sessions(&self) -> ImHashSet<String> {
        self.core.sessions()
    }

    /// 当前活跃 session 数量
    pub fn session_count(&self) -> usize {
        self.core.session_count()
    }

    /// 获取最后活动时间的便捷方法
    pub fn last_activity(&self) -> DateTime<Utc> {
        self.core.last_activity
    }
}

/// 为了向后兼容，保留原有的 ProjectAndContainerInfo 结构
///
/// 内部使用新的 ProjectState，但保持相同的 API 接口
#[derive(Clone)]
pub struct ProjectAndContainerInfo {
    /// 内部状态管理
    state: ProjectState,
}

impl ProjectAndContainerInfo {
    pub fn new(project_id: String) -> Self {
        Self {
            state: ProjectState::new(project_id),
        }
    }

    /// 从各部分构造（主要用于测试）
    #[allow(dead_code)]
    pub fn from_parts(
        project_id: String,
        user_id: Option<String>,
        pod_id: Option<String>,
        session_id: Option<String>,
        container: Option<ContainerBasicInfo>,
        fields: ProjectExtendedFields,
    ) -> Self {
        let now = Utc::now();
        // 兼容旧签名：session_id 参数被加入 sessions 集合
        let mut sessions = ImHashSet::new();
        if let Some(ref sid) = session_id {
            sessions.insert(sid.clone());
        }
        let core = ProjectCoreState {
            project_id,
            user_id,
            pod_id,
            tenant_id: fields.tenant_id,
            space_id: fields.space_id,
            isolation_type: fields.isolation_type,
            sessions: Arc::new(sessions),
            latest_session: session_id,
            last_activity: fields.last_activity.unwrap_or(now),
            created_at: fields.created_at.unwrap_or(now),
        };
        let mut extended = ProjectExtendedState::new();
        extended.request_id = fields.request_id;
        extended.service_type = fields.service_type;
        let mut info = Self {
            state: ProjectState {
                core: Arc::new(core),
                extended: Arc::new(extended),
            },
        };
        // container 包装成 Arc<ContainerEntry>（此时 service_type / container_key 已就绪）
        info.set_container(container);
        info
    }

    /// 添加 session（C2 修复核心 API）
    ///
    /// - 不清除其他 session（多 session 并存）
    /// - 更新 latest_session
    /// - 更新 last_activity
    pub fn add_session(&mut self, session_id: impl Into<String>) {
        self.state.update_core(|core| {
            core.add_session(session_id);
        });
    }

    /// 恢复专用（boot 加载 / sync 重建 / 回源 hydrate）：集合语义同
    /// [`Self::add_session`]，但不触碰 last_activity——恢复动作不代表用户
    /// 活跃，时间戳以持久化行为准（否则重启/回源即把 idle 计时归零）。
    pub fn restore_session(&mut self, session_id: impl Into<String>) {
        self.state.update_core(|core| {
            core.restore_session(session_id);
        });
    }

    /// 移除指定 session
    ///
    /// 返回 true 表示该 session 之前存在。若移除的是 latest，自动重选。
    pub fn remove_session(&mut self, session_id: &str) -> bool {
        let mut removed = false;
        self.state.update_core(|core| {
            removed = core.remove_session(session_id);
        });
        removed
    }

    /// 返回 sessions 廉价快照
    pub fn sessions(&self) -> ImHashSet<String> {
        self.state.sessions()
    }

    /// 当前活跃 session 数量
    pub fn session_count(&self) -> usize {
        self.state.session_count()
    }

    /// 清空所有 session
    pub fn clear_all_sessions(&mut self) {
        self.state.update_core(|core| {
            core.clear_all_sessions();
        });
    }

    /// 高效更新核心状态（已废弃，转发到 add_session）
    #[deprecated(
        since = "0.0.0",
        note = "use `add_session` instead - 多 session 模型不再覆盖"
    )]
    pub fn update_session(&mut self, session_id: String) {
        self.add_session(session_id);
    }

    /// 高效更新活动时间
    pub fn update_activity(&mut self) {
        self.state.update_core(|core| {
            core.update_activity();
        });
    }

    /// 批量更新扩展状态
    pub fn update_extended_from_request(
        &mut self,
        container: Option<ContainerBasicInfo>,
        model_provider: Option<ModelProviderConfig>,
        request_id: Option<String>,
        service_type: Option<ServiceType>,
    ) {
        // 容器信息包装成 Arc<ContainerEntry>（service_type 用入参或现有值）
        let entry = container.map(|c| {
            let st = service_type
                .clone()
                .or(self.service_type())
                .unwrap_or(ServiceType::WebAgentRunner);
            let logical_id = self.container_key().to_string();
            Arc::new(ContainerEntry::new(c, st, logical_id))
        });
        self.state.update_extended(|extended| {
            extended.update_from_request(entry, model_provider, request_id, service_type);
        });
    }
}

// ========== 为了向后兼容保留的访问器 ==========
impl ProjectAndContainerInfo {
    pub fn project_id(&self) -> &str {
        self.state.project_id()
    }

    /// 获取用户ID（ComputerAgentRunner 模式专用）
    pub fn user_id(&self) -> Option<&str> {
        self.state.core.user_id.as_deref()
    }

    /// 获取 Pod ID（共享容器模式）
    pub fn pod_id(&self) -> Option<&str> {
        self.state.core.pod_id.as_deref()
    }

    /// 获取租户 ID（共享容器隔离下项目所属租户）
    pub fn tenant_id(&self) -> Option<&str> {
        self.state.core.tenant_id.as_deref()
    }

    /// 获取空间 ID（共享容器隔离下项目所属空间）
    pub fn space_id(&self) -> Option<&str> {
        self.state.core.space_id.as_deref()
    }

    /// 获取隔离类型（tenant/space/project）
    pub fn isolation_type(&self) -> Option<&str> {
        self.state.core.isolation_type.as_deref()
    }

    /// 获取容器唯一标识
    ///
    /// 根据 service_type 返回不同的标识符：
    /// - WebAgentRunner 模式：返回 pod_id（如果存在，共享容器），否则返回 project_id
    /// - ComputerAgentRunner 模式：返回 pod_id（共享容器）→ user_id（per-user 容器）→ project_id（兜底）
    ///
    /// ## 重要说明
    ///
    /// `ComputerAgentRunner` 默认用 `user_id`（无 pod_id 时容器由 user_id 确认）；
    /// 若提供 `pod_id`，则多个 user 可共享同一容器（pod_id 作为 container_key，
    /// 与容器创建侧 `agent_container_starter` 的 container_id 选择一致）。
    /// `WebAgentRunner` 用 `pod_id` 或 `project_id`，不使用 `user_id`。
    pub fn container_key(&self) -> &str {
        match self.service_type() {
            Some(ServiceType::ComputerAgentRunner) => self
                .pod_id()
                .or_else(|| self.user_id())
                .unwrap_or_else(|| self.project_id()),
            // WebAgentRunner 或 service_type 未设置：使用 pod_id 或 project_id
            _ => self.pod_id().unwrap_or_else(|| self.project_id()),
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.state.session_id()
    }

    /// 返回最新添加的 session_id（与 `session_id()` 等价，语义更明确）
    pub fn latest_session(&self) -> Option<&str> {
        self.state.core.latest_session.as_deref()
    }

    pub fn last_activity(&self) -> DateTime<Utc> {
        self.state.last_activity()
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.state.core.created_at
    }

    pub fn model_provider(&self) -> Option<&ModelProviderConfig> {
        self.state.extended.model_provider.as_ref()
    }

    /// 获取共享的容器条目引用（与 `ProjectAdapter.containers[name]` 同一 Arc 实例）。
    /// 需要容器字段值时优先用 `container_info()`（owned clone）。
    pub fn container(&self) -> Option<&Arc<ContainerEntry>> {
        self.state.extended.container.as_ref()
    }

    /// 获取容器信息的 owned 克隆（从共享 ContainerEntry 读出）。
    /// 供只需 container_id / container_ip 等字段值的调用方使用。
    pub fn container_info(&self) -> Option<ContainerBasicInfo> {
        self.container().map(|e| e.info())
    }

    pub fn request_id(&self) -> Option<&str> {
        self.state.extended.request_id.as_deref()
    }

    pub fn status(&self) -> Option<&AgentStatus> {
        self.state.extended.status.as_ref()
    }

    pub fn service_type(&self) -> Option<ServiceType> {
        self.state.extended.service_type.clone()
    }

    // ========== 可变访问器（会触发写时复制） ==========

    /// 设置 session_id（已废弃，转发到 add_session）
    ///
    /// 历史语义：`set_session_id(Some(x))` 等价"覆盖为 x"。
    /// 新模型下转发为 `add_session(x)` —— **不再清除其他 session**。
    /// 若调用方依赖"覆盖"语义（清除旧 session），应显式调 `clear_all_sessions()` 后再 `add_session`。
    #[deprecated(
        since = "0.0.0",
        note = "use `add_session` instead - 多 session 模型不再覆盖"
    )]
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        if let Some(session_id) = session_id {
            self.add_session(session_id);
        }
    }

    /// 清除所有 session（已废弃，新代码请用 `clear_all_sessions`）
    ///
    /// 历史语义：清除单一 session_id。
    /// 新模型下"清除单一"已不适用，直接清空全部。
    #[deprecated(since = "0.0.0", note = "use `clear_all_sessions` instead")]
    pub fn clear_session_id(&mut self) {
        self.clear_all_sessions();
    }

    /// 设置用户ID（ComputerAgentRunner 模式专用）
    pub fn set_user_id(&mut self, user_id: Option<String>) {
        self.state.update_core(|core| {
            core.user_id = user_id;
        });
    }

    /// 设置项目归属 scope（tenant/space/isolation）。
    ///
    /// 共享容器（tenant/space 隔离）下，终端 cwd 等运行时查询需要按 project_id 反查
    /// 这三个值（见 `ContainerLookup::find_project_scope`）。scope 是 project 创建时
    /// 确定的稳定属性，重复 set 幂等无害。走 `update_core` 写时复制。
    pub fn set_scope(
        &mut self,
        tenant_id: Option<String>,
        space_id: Option<String>,
        isolation_type: Option<String>,
    ) {
        self.state.update_core(|core| {
            core.tenant_id = tenant_id;
            core.space_id = space_id;
            core.isolation_type = isolation_type;
        });
    }

    pub fn set_pod_id(&mut self, pod_id: Option<String>) {
        self.state.update_core(|core| {
            core.pod_id = pod_id;
        });
    }

    pub fn set_model_provider(&mut self, model_provider: Option<ModelProviderConfig>) {
        self.state.update_extended(|extended| {
            extended.model_provider = model_provider;
        });
    }

    /// 设置容器信息（接收裸 ContainerBasicInfo，内部包成 `Arc<ContainerEntry>`）。
    /// Arc 在 insert 时与 `ProjectAdapter.containers[name]` 共享同一实例。
    pub fn set_container(&mut self, container: Option<ContainerBasicInfo>) {
        let entry = container.map(|c| {
            let st = self.service_type().unwrap_or(ServiceType::WebAgentRunner);
            let logical_id = self.container_key().to_string();
            Arc::new(ContainerEntry::new(c, st, logical_id))
        });
        self.state.update_extended(|extended| {
            extended.container = entry;
        });
    }

    /// 直接设置共享的 `Arc<ContainerEntry>`（insert 在 Occupied/重建场景回写权威 Arc 用）。
    pub fn set_container_arc(&mut self, entry: Option<Arc<ContainerEntry>>) {
        self.state.update_extended(|extended| {
            extended.container = entry;
        });
    }

    pub fn set_request_id(&mut self, request_id: Option<String>) {
        self.state.update_extended(|extended| {
            extended.request_id = request_id;
        });
    }

    pub fn set_status(&mut self, status: Option<AgentStatus>) {
        self.state.update_extended(|extended| {
            extended.status = status;
        });
    }

    pub fn set_service_type(&mut self, service_type: Option<ServiceType>) {
        self.state.update_extended(|extended| {
            if let Some(st) = service_type {
                extended.service_type = Some(st);
            }
        });
    }

    /// 设置时间戳（用于从持久化存储恢复数据）
    ///
    /// 当从持久化存储读取数据时，需要恢复原始的时间戳，
    /// 而不是使用 `new()` 中设置的当前时间。
    ///
    /// # Arguments
    /// * `created_at` - 创建时间
    /// * `last_activity` - 最后活动时间
    pub fn set_timestamps(&mut self, created_at: DateTime<Utc>, last_activity: DateTime<Utc>) {
        self.state.update_core(|core| {
            core.created_at = created_at;
            core.last_activity = last_activity;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_set_add_and_latest() {
        let mut info = ProjectAndContainerInfo::new("p1".into());
        assert_eq!(info.session_count(), 0);
        assert!(info.session_id().is_none());
        assert!(info.latest_session().is_none());

        info.add_session("s1");
        assert_eq!(info.session_count(), 1);
        assert_eq!(info.latest_session(), Some("s1"));

        info.add_session("s2");
        assert_eq!(info.session_count(), 2);
        assert_eq!(info.latest_session(), Some("s2"));
        // session_id() 兼容读路径返回 latest
        assert_eq!(info.session_id(), Some("s2"));

        // sessions 快照包含两个
        let snapshot = info.sessions();
        assert!(snapshot.contains("s1"));
        assert!(snapshot.contains("s2"));
    }

    #[test]
    fn test_container_key_by_service_type() {
        // ComputerAgentRunner 无 pod_id：用 user_id（per-user 容器）
        let mut comp = ProjectAndContainerInfo::new("proj-1".into());
        comp.set_service_type(Some(ServiceType::ComputerAgentRunner));
        comp.set_user_id(Some("user-6".into()));
        assert_eq!(comp.container_key(), "user-6");

        // ComputerAgentRunner 有 pod_id：用 pod_id（共享容器，跨 user 复用）
        comp.set_pod_id(Some("pod-shared".into()));
        assert_eq!(comp.container_key(), "pod-shared");

        // WebAgentRunner 无 pod_id：用 project_id
        let mut web = ProjectAndContainerInfo::new("proj-web".into());
        web.set_service_type(Some(ServiceType::WebAgentRunner));
        assert_eq!(web.container_key(), "proj-web");

        // WebAgentRunner 有 pod_id：用 pod_id（共享容器）
        web.set_pod_id(Some("pod-web".into()));
        assert_eq!(web.container_key(), "pod-web");
    }

    #[test]
    fn test_session_remove_and_latest_fallback() {
        let mut info = ProjectAndContainerInfo::new("p1".into());
        info.add_session("s1");
        info.add_session("s2");
        // latest 是 s2

        let removed = info.remove_session("s2");
        assert!(removed);
        assert_eq!(info.session_count(), 1);
        // 移除 latest 后退化到 s1
        assert_eq!(info.latest_session(), Some("s1"));

        // 移除最后一个
        let removed2 = info.remove_session("s1");
        assert!(removed2);
        assert_eq!(info.session_count(), 0);
        assert!(info.latest_session().is_none());

        // 移除不存在的
        let removed3 = info.remove_session("nonexistent");
        assert!(!removed3);
    }

    #[test]
    fn test_clear_all_sessions() {
        let mut info = ProjectAndContainerInfo::new("p1".into());
        info.add_session("s1");
        info.add_session("s2");
        info.add_session("s3");
        assert_eq!(info.session_count(), 3);

        info.clear_all_sessions();
        assert_eq!(info.session_count(), 0);
        assert!(info.latest_session().is_none());
        assert!(info.sessions().is_empty());
    }

    /// 共享验证：Arc<im::HashSet> 的 clone 是 O(1)（仅 Arc 引用计数 bump）
    #[test]
    fn test_sessions_snapshot_is_cheap_clone() {
        let mut info = ProjectAndContainerInfo::new("p1".into());
        for i in 0..100 {
            info.add_session(format!("s{}", i));
        }
        // 多次 clone 快照应该廉价
        let snap1 = info.sessions();
        let snap2 = info.sessions();
        let snap3 = info.sessions();
        assert_eq!(snap1.len(), 100);
        assert_eq!(snap2.len(), 100);
        assert_eq!(snap3.len(), 100);
    }

    /// Arc::make_mut 行为：ref_count > 1 时 clone，否则原地修改
    #[test]
    fn test_core_state_make_mut_behavior() {
        let mut info = ProjectAndContainerInfo::new("p1".into());
        info.add_session("s1");

        // 通过 update_core 修改时，Arc 内部 make_mut
        info.add_session("s2");
        assert_eq!(info.session_count(), 2);
    }

    /// from_parts 兼容性：旧 session_id 参数自动转入 sessions 集合
    #[test]
    fn test_from_parts_legacy_session_id_arg() {
        let info = ProjectAndContainerInfo::from_parts(
            "p1".into(),
            None,
            None,
            Some("legacy-session".into()),
            None,
            ProjectExtendedFields::default(),
        );
        assert_eq!(info.session_count(), 1);
        assert_eq!(info.latest_session(), Some("legacy-session"));
        assert_eq!(info.session_id(), Some("legacy-session"));
    }
}

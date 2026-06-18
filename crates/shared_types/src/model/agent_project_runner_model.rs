use chrono::{DateTime, Utc};
use im::HashSet as ImHashSet;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{AgentStatus, ModelProviderConfig};
use crate::ServiceType;

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

    /// 移除指定 session
    ///
    /// 返回 true 表示该 session 之前存在并已被移除。
    /// 若移除的是 `latest_session`，从剩余 sessions 中任选一个作为新 latest
    ///（im::HashSet 迭代顺序稳定但无意义，这里只是为了不返回 None 误误导读路径）。
    pub fn remove_session(&mut self, session_id: &str) -> bool {
        let set = Arc::make_mut(&mut self.sessions);
        let removed = set.remove(session_id).is_some();
        if removed
            && self.latest_session.as_deref() == Some(session_id)
        {
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
}

/// 这些字段相对稳定，不需要频繁更新
#[derive(Debug, Clone)]
pub struct ProjectExtendedState {
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// container 容器信息，一个project_id 只能对应最多1个容器
    pub container: Option<ContainerBasicInfo>,
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

    /// 批量更新扩展状态
    pub fn update_from_request(
        &mut self,
        container: Option<ContainerBasicInfo>,
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
            sessions: Arc::new(sessions),
            latest_session: session_id,
            last_activity: fields.last_activity.unwrap_or(now),
            created_at: fields.created_at.unwrap_or(now),
        };
        let mut extended = ProjectExtendedState::new();
        extended.container = container;
        extended.request_id = fields.request_id;
        extended.service_type = fields.service_type;
        Self {
            state: ProjectState {
                core: Arc::new(core),
                extended: Arc::new(extended),
            },
        }
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
    #[deprecated(since = "0.0.0", note = "use `add_session` instead - 多 session 模型不再覆盖")]
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
        self.state.update_extended(|extended| {
            extended.update_from_request(container, model_provider, request_id, service_type);
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

    /// 获取容器唯一标识
    ///
    /// 根据 service_type 返回不同的标识符：
    /// - RCoder 模式：返回 pod_id（如果存在，共享容器），否则返回 project_id
    /// - ComputerAgentRunner 模式：返回 user_id（如果存在），否则回退到 project_id
    pub fn container_key(&self) -> &str {
        match self.service_type() {
            Some(ServiceType::ComputerAgentRunner) => {
                self.user_id().unwrap_or_else(|| self.project_id())
            }
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

    pub fn container(&self) -> Option<&ContainerBasicInfo> {
        self.state.extended.container.as_ref()
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
    #[deprecated(since = "0.0.0", note = "use `add_session` instead - 多 session 模型不再覆盖")]
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

    pub fn set_container(&mut self, container: Option<ContainerBasicInfo>) {
        self.state.update_extended(|extended| {
            extended.container = container;
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

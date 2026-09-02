//! 状态内核（自 agent_project_runner_model 拆出）：高频 Core + 稳定 Extended 的
//! CoW 组合（ProjectState），session 集合经 imbl 持久化结构做结构共享。

use chrono::{DateTime, Utc};
use imbl::HashSet as ImHashSet;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::{AgentStatus, ModelProviderConfig};
use super::container_info::ContainerBasicInfo;
use crate::{ContainerEntry, ServiceType};

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

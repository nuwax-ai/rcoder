use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::ServiceType;
use super::{AgentStatus, ModelProviderConfig};

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
    /// 会话ID，agent 服务启动时会创建一个会话ID
    pub session_id: Option<String>,
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
            session_id: None,
            last_activity: now,
            created_at: now,
        }
    }

    /// 更新会话信息 - 高频操作
    pub fn update_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.last_activity = Utc::now();
    }

    /// 更新活动时间 - 高频操作
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }
}

/// 项目扩展状态 - 包含较少变更的大字段
///
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

    /// 获取会话ID的便捷方法
    pub fn session_id(&self) -> Option<&str> {
        self.core.session_id.as_deref()
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

    /// 高效更新核心状态 - 新的推荐方法
    pub fn update_session(&mut self, session_id: String) {
        self.state.update_core(|core| {
            core.update_session(session_id);
        });
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

    pub fn session_id(&self) -> Option<&str> {
        self.state.session_id()
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

    pub fn set_session_id(&mut self, session_id: Option<String>) {
        if let Some(session_id) = session_id {
            self.update_session(session_id);
        }
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
}

use agent_client_protocol::SessionId;
use chrono::{DateTime, Utc};
use docker_manager::ContainerBasicInfo;

use super::{AgentStatus, ModelProviderConfig};

/// 项目id与 Container 服务池，一个项目对应一个 Container 服务
///
/// Clone trait 是必需的，因为 DashMap::insert() 要求值类型实现 Clone
#[derive(Clone)]
pub struct ProjectAndContainerInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID，agent 服务启动时会创建一个会话ID
    pub session_id: Option<String>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    ///container 容器,一个project_id 只能对应最多1个容器 container 容器
    pub container: Option<ContainerBasicInfo>,
    /// 当前活跃的请求ID，用于标识用户请求
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: Option<AgentStatus>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
}

impl ProjectAndContainerInfo {
    pub fn new(project_id: String) -> Self {
        Self {
            project_id,
            session_id: None,
            model_provider: None,
            container: None,
            request_id: None,
            status: None,
            last_activity: Utc::now(),
            created_at: Utc::now(),
        }
    }
}

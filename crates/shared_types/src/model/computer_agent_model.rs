//! Computer Agent Runner 核心数据模型
//!
//! 本模块定义了 Computer Agent Runner 功能所需的核心数据结构，
//! 使用统一架构管理 RCoder 和 ComputerAgentRunner 两种模式的容器。
//!
//! ## 核心设计原则
//! - **统一容器标识**: 使用 `ContainerKey` 枚举区分 Project 和 User 两种容器标识模式
//! - **统一容器信息**: 使用 `UnifiedContainerInfo` 合并两种模式的容器信息
//! - **减少映射数量**: 从 6 个 DashMap 精简到 3 个核心映射
//!
//! ## 两种模式对比
//! | 维度 | RCoder | ComputerAgentRunner |
//! |------|--------|---------------------|
//! | 容器标识 | `project_id` | `user_id` |
//! | Agent 实例数 | 1 个 | 多个（按 `project_id` 区分） |
//! | 闲置策略 | project_id 闲置即销毁 | user_id 下所有 project_id 都闲置才销毁 |

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use super::agent_project_runner_model::ContainerBasicInfo;
use super::{AgentStatus, ModelProviderConfig};
use crate::ServiceType;

// ============================================================================
// ContainerKey - 统一容器标识符
// ============================================================================

/// 统一的容器标识符
///
/// 用于区分 RCoder 和 ComputerAgentRunner 两种模式的容器。
/// - `Project(String)`: RCoder 模式，一个 project_id 对应一个容器
/// - `User(String)`: ComputerAgentRunner 模式，一个 user_id 对应一个容器
///
/// # 示例
/// ```ignore
/// use shared_types::ContainerKey;
///
/// // RCoder 模式
/// let key1 = ContainerKey::from_project("proj_123".to_string());
/// assert_eq!(key1.to_string(), "project:proj_123");
///
/// // ComputerAgentRunner 模式
/// let key2 = ContainerKey::from_user("user_456".to_string());
/// assert_eq!(key2.to_string(), "user:user_456");
/// ```
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContainerKey {
    /// RCoder 模式：一个 project_id 对应一个容器
    Project(String),

    /// ComputerAgentRunner 模式：一个 user_id 对应一个容器
    User(String),
}

impl Hash for ContainerKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // 先 hash 变体类型
        std::mem::discriminant(self).hash(state);
        // 再 hash 内部值
        match self {
            ContainerKey::Project(id) => id.hash(state),
            ContainerKey::User(id) => id.hash(state),
        }
    }
}

impl ContainerKey {
    /// 获取容器标识符的字符串形式（用于 Docker 容器查询）
    ///
    /// 返回内部的 ID 字符串，不包含前缀。
    pub fn as_str(&self) -> &str {
        match self {
            ContainerKey::Project(id) => id,
            ContainerKey::User(id) => id,
        }
    }

    /// 获取对应的 ServiceType
    pub fn service_type(&self) -> ServiceType {
        match self {
            ContainerKey::Project(_) => ServiceType::RCoder,
            ContainerKey::User(_) => ServiceType::ComputerAgentRunner,
        }
    }

    /// 从 project_id 创建（RCoder 模式）
    pub fn from_project(project_id: String) -> Self {
        ContainerKey::Project(project_id)
    }

    /// 从 user_id 创建（ComputerAgentRunner 模式）
    pub fn from_user(user_id: String) -> Self {
        ContainerKey::User(user_id)
    }

    /// 检查是否是 Project 类型
    pub fn is_project(&self) -> bool {
        matches!(self, ContainerKey::Project(_))
    }

    /// 检查是否是 User 类型
    pub fn is_user(&self) -> bool {
        matches!(self, ContainerKey::User(_))
    }
}

impl std::fmt::Display for ContainerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerKey::Project(id) => write!(f, "project:{}", id),
            ContainerKey::User(id) => write!(f, "user:{}", id),
        }
    }
}

// ============================================================================
// ProjectInfo - 项目信息（ComputerAgentRunner 模式）
// ============================================================================

/// 项目信息
///
/// 用于 ComputerAgentRunner 模式下管理一个用户容器内的多个项目。
/// 每个项目对应一个独立的 AI Agent 实例。
///
/// # 示例
/// ```ignore
/// use shared_types::computer_agent_model::ProjectInfo;
///
/// let mut project = ProjectInfo::new("proj_123".to_string());
/// project.update_session("session_456".to_string());
/// ```
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// 项目 ID
    pub project_id: String,

    /// 会话 ID（当前活跃的会话）
    pub session_id: Option<String>,

    /// Agent 服务状态
    pub status: Option<AgentStatus>,

    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 创建时间
    pub created_at: DateTime<Utc>,

    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
}

impl ProjectInfo {
    /// 创建新的项目信息
    pub fn new(project_id: String) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            session_id: None,
            status: None,
            model_provider: None,
            created_at: now,
            last_activity: now,
        }
    }

    /// 更新活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    /// 更新会话信息
    pub fn update_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.update_activity();
    }

    /// 更新 Agent 状态
    pub fn update_status(&mut self, status: AgentStatus) {
        self.status = Some(status);
        self.update_activity();
    }

    /// 设置模型提供商配置
    pub fn set_model_provider(&mut self, model_provider: ModelProviderConfig) {
        self.model_provider = Some(model_provider);
    }

    /// 检查项目是否闲置
    ///
    /// 闲置条件：状态为 Idle 或 None，且超过指定的超时时间
    pub fn is_idle(&self, idle_timeout: Duration) -> bool {
        let now = Utc::now();
        let idle_duration = now - self.last_activity;
        let is_timeout = idle_duration
            > ChronoDuration::from_std(idle_timeout).unwrap_or(ChronoDuration::MAX);

        let is_idle_status = matches!(self.status, Some(AgentStatus::Idle) | None);
        is_idle_status && is_timeout
    }
}

// ============================================================================
// SessionInfo - 会话信息
// ============================================================================

/// 会话信息
///
/// 统一管理 RCoder 和 ComputerAgentRunner 的会话映射。
/// 用于通过 session_id 快速定位到对应的容器和项目。
///
/// # 用途
/// - SSE 进度流通过 session_id 找到对应的容器
/// - 取消操作通过 session_id 定位到对应的 Agent
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// 会话 ID
    pub session_id: String,

    /// 容器标识符（关联到 containers 映射）
    pub container_key: ContainerKey,

    /// 项目 ID
    /// - RCoder 模式：与 container_key 中的 project_id 相同
    /// - ComputerAgentRunner 模式：容器内的具体项目 ID
    pub project_id: String,

    /// 创建时间
    pub created_at: DateTime<Utc>,
}

impl SessionInfo {
    /// 创建新的会话信息
    pub fn new(session_id: String, container_key: ContainerKey, project_id: String) -> Self {
        Self {
            session_id,
            container_key,
            project_id,
            created_at: Utc::now(),
        }
    }
}

// ============================================================================
// UnifiedContainerInfo - 统一容器信息
// ============================================================================

/// 统一的容器信息结构
///
/// 同时支持 RCoder 和 ComputerAgentRunner 两种模式。
/// 使用 Option 字段区分模式特定的数据。
///
/// # 设计原则
/// - 合并两种模式的信息，减少代码重复
/// - 使用 Option 字段实现模式特定数据的区分
/// - 提供统一的 `is_fully_idle()` 方法自动处理两种模式的闲置判断
///
/// # RCoder 模式 vs ComputerAgentRunner 模式
///
/// | 字段 | RCoder | ComputerAgentRunner |
/// |------|--------|---------------------|
/// | session_id | 使用 | 不使用（由 ProjectInfo 管理） |
/// | status | 使用 | 不使用（由 ProjectInfo 管理） |
/// | model_provider | 使用 | 不使用（由 ProjectInfo 管理） |
/// | projects | 不使用 | 使用 |
#[derive(Debug, Clone)]
pub struct UnifiedContainerInfo {
    /// 容器标识符（区分模式）
    pub key: ContainerKey,

    /// 容器基本信息
    pub container: ContainerBasicInfo,

    /// 服务类型
    pub service_type: ServiceType,

    /// 容器创建时间
    pub created_at: DateTime<Utc>,

    /// 最后活动时间（容器级别）
    pub last_activity: DateTime<Utc>,

    // ========== RCoder 模式字段 ==========
    /// RCoder 模式：当前会话 ID
    pub session_id: Option<String>,

    /// RCoder 模式：Agent 状态
    pub status: Option<AgentStatus>,

    /// RCoder 模式：模型配置
    pub model_provider: Option<ModelProviderConfig>,

    // ========== ComputerAgentRunner 模式字段 ==========
    /// ComputerAgentRunner 模式：容器内的所有项目映射
    /// key: project_id, value: ProjectInfo
    pub projects: Option<Arc<DashMap<String, Arc<ProjectInfo>>>>,
}

impl UnifiedContainerInfo {
    /// 创建 RCoder 模式的容器信息
    ///
    /// # 参数
    /// - `project_id`: 项目 ID（作为容器标识）
    /// - `container`: 容器基本信息
    pub fn new_rcoder(project_id: String, container: ContainerBasicInfo) -> Self {
        let now = Utc::now();
        Self {
            key: ContainerKey::Project(project_id),
            container,
            service_type: ServiceType::RCoder,
            created_at: now,
            last_activity: now,
            session_id: None,
            status: None,
            model_provider: None,
            projects: None,
        }
    }

    /// 创建 ComputerAgentRunner 模式的容器信息
    ///
    /// # 参数
    /// - `user_id`: 用户 ID（作为容器标识）
    /// - `container`: 容器基本信息
    pub fn new_computer(user_id: String, container: ContainerBasicInfo) -> Self {
        let now = Utc::now();
        Self {
            key: ContainerKey::User(user_id),
            container,
            service_type: ServiceType::ComputerAgentRunner,
            created_at: now,
            last_activity: now,
            session_id: None,
            status: None,
            model_provider: None,
            projects: Some(Arc::new(DashMap::new())),
        }
    }

    /// 更新活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    // ========== ComputerAgentRunner 专用方法 ==========

    /// 添加或更新项目（仅 ComputerAgentRunner 模式）
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_info`: 项目信息
    pub fn upsert_project(&self, project_id: String, project_info: Arc<ProjectInfo>) {
        if let Some(projects) = &self.projects {
            projects.insert(project_id, project_info);
        }
    }

    /// 获取项目（仅 ComputerAgentRunner 模式）
    pub fn get_project(&self, project_id: &str) -> Option<Arc<ProjectInfo>> {
        self.projects.as_ref()?.get(project_id).map(|r| r.clone())
    }

    /// 移除项目（仅 ComputerAgentRunner 模式）
    pub fn remove_project(&self, project_id: &str) -> Option<Arc<ProjectInfo>> {
        self.projects.as_ref()?.remove(project_id).map(|(_, v)| v)
    }

    /// 列出所有项目 ID（仅 ComputerAgentRunner 模式）
    pub fn list_projects(&self) -> Vec<String> {
        self.projects
            .as_ref()
            .map(|p| p.iter().map(|r| r.key().clone()).collect())
            .unwrap_or_default()
    }

    /// 获取项目数量（仅 ComputerAgentRunner 模式）
    pub fn project_count(&self) -> usize {
        self.projects.as_ref().map(|p| p.len()).unwrap_or(0)
    }

    // ========== 统一方法 ==========

    /// 检查容器是否完全闲置
    ///
    /// 闲置判断逻辑：
    /// - **RCoder 模式**: 检查 status 是否为 Idle 且超过超时时间
    /// - **ComputerAgentRunner 模式**: 检查所有项目是否都闲置且超过超时时间
    ///
    /// # 参数
    /// - `idle_timeout`: 闲置超时时间
    ///
    /// # 返回
    /// - `true`: 容器完全闲置，可以清理
    /// - `false`: 容器仍有活跃任务
    pub fn is_fully_idle(&self, idle_timeout: Duration) -> bool {
        let now = Utc::now();
        let idle_duration = now - self.last_activity;
        let is_timeout = idle_duration
            > ChronoDuration::from_std(idle_timeout).unwrap_or(ChronoDuration::MAX);

        match self.service_type {
            ServiceType::RCoder => {
                // RCoder 模式：检查自身状态
                let is_idle_status = matches!(self.status, Some(AgentStatus::Idle) | None);
                is_idle_status && is_timeout
            }
            ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 模式：检查所有项目
                if let Some(projects) = &self.projects {
                    // 没有项目，可以清理
                    if projects.is_empty() {
                        return true;
                    }

                    // 所有项目都必须闲置
                    projects.iter().all(|entry| {
                        let project_info = entry.value();
                        project_info.is_idle(idle_timeout)
                    })
                } else {
                    true
                }
            }
        }
    }

    /// 获取容器 IP
    pub fn container_ip(&self) -> &str {
        &self.container.container_ip
    }

    /// 获取容器 ID
    pub fn container_id(&self) -> &str {
        &self.container.container_id
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_container_basic_info(project_id: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: format!("container_{}", project_id),
            container_name: format!("rcoder-agent-{}", project_id),
            container_ip: "172.17.0.2".to_string(),
            internal_port: 8086,
            external_port: 8086,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://172.17.0.2:8086"),
        }
    }

    #[test]
    fn test_container_key_project() {
        let key = ContainerKey::from_project("proj_123".to_string());
        assert_eq!(key.as_str(), "proj_123");
        assert_eq!(key.service_type(), ServiceType::RCoder);
        assert!(key.is_project());
        assert!(!key.is_user());
        assert_eq!(key.to_string(), "project:proj_123");
    }

    #[test]
    fn test_container_key_user() {
        let key = ContainerKey::from_user("user_456".to_string());
        assert_eq!(key.as_str(), "user_456");
        assert_eq!(key.service_type(), ServiceType::ComputerAgentRunner);
        assert!(key.is_user());
        assert!(!key.is_project());
        assert_eq!(key.to_string(), "user:user_456");
    }

    #[test]
    fn test_container_key_hash_equality() {
        let key1 = ContainerKey::from_project("proj_123".to_string());
        let key2 = ContainerKey::from_project("proj_123".to_string());
        let key3 = ContainerKey::from_user("proj_123".to_string());

        assert_eq!(key1, key2);
        assert_ne!(key1, key3); // 即使 ID 相同，类型不同也不相等
    }

    #[test]
    fn test_project_info_new() {
        let project = ProjectInfo::new("proj_123".to_string());
        assert_eq!(project.project_id, "proj_123");
        assert!(project.session_id.is_none());
        assert!(project.status.is_none());
        assert!(project.model_provider.is_none());
    }

    #[test]
    fn test_project_info_update_session() {
        let mut project = ProjectInfo::new("proj_123".to_string());
        let old_activity = project.last_activity;

        // 等待一小段时间确保时间戳变化
        std::thread::sleep(std::time::Duration::from_millis(10));

        project.update_session("session_456".to_string());
        assert_eq!(project.session_id, Some("session_456".to_string()));
        assert!(project.last_activity > old_activity);
    }

    #[test]
    fn test_project_info_is_idle() {
        let mut project = ProjectInfo::new("proj_123".to_string());

        // 刚创建的项目，status 为 None，但还没超时
        assert!(!project.is_idle(Duration::from_secs(60)));

        // 设置为 Idle 状态
        project.status = Some(AgentStatus::Idle);

        // 短超时时间内不闲置
        assert!(!project.is_idle(Duration::from_secs(3600)));

        // 超长超时时间（0秒）应该闲置
        assert!(project.is_idle(Duration::from_secs(0)));
    }

    #[test]
    fn test_session_info_new() {
        let key = ContainerKey::from_user("user_123".to_string());
        let session = SessionInfo::new(
            "session_456".to_string(),
            key.clone(),
            "proj_789".to_string(),
        );

        assert_eq!(session.session_id, "session_456");
        assert_eq!(session.container_key, key);
        assert_eq!(session.project_id, "proj_789");
    }

    #[test]
    fn test_unified_container_info_rcoder() {
        let container = create_mock_container_basic_info("proj_123");
        let info = UnifiedContainerInfo::new_rcoder("proj_123".to_string(), container);

        assert!(matches!(info.key, ContainerKey::Project(_)));
        assert_eq!(info.service_type, ServiceType::RCoder);
        assert!(info.projects.is_none());
        assert_eq!(info.container_ip(), "172.17.0.2");
    }

    #[test]
    fn test_unified_container_info_computer() {
        let container = create_mock_container_basic_info("user_123");
        let info = UnifiedContainerInfo::new_computer("user_123".to_string(), container);

        assert!(matches!(info.key, ContainerKey::User(_)));
        assert_eq!(info.service_type, ServiceType::ComputerAgentRunner);
        assert!(info.projects.is_some());
        assert_eq!(info.project_count(), 0);
    }

    #[test]
    fn test_unified_container_info_projects_crud() {
        let container = create_mock_container_basic_info("user_123");
        let info = UnifiedContainerInfo::new_computer("user_123".to_string(), container);

        // 添加项目
        let proj1 = Arc::new(ProjectInfo::new("proj_1".to_string()));
        let proj2 = Arc::new(ProjectInfo::new("proj_2".to_string()));

        info.upsert_project("proj_1".to_string(), proj1);
        info.upsert_project("proj_2".to_string(), proj2);

        assert_eq!(info.project_count(), 2);

        // 获取项目
        let retrieved = info.get_project("proj_1");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().project_id, "proj_1");

        // 列出项目
        let projects = info.list_projects();
        assert_eq!(projects.len(), 2);

        // 移除项目
        let removed = info.remove_project("proj_1");
        assert!(removed.is_some());
        assert_eq!(info.project_count(), 1);
    }

    #[test]
    fn test_unified_container_info_is_fully_idle_rcoder() {
        let container = create_mock_container_basic_info("proj_123");
        let mut info = UnifiedContainerInfo::new_rcoder("proj_123".to_string(), container);

        // 刚创建，status 为 None，短超时不闲置
        assert!(!info.is_fully_idle(Duration::from_secs(3600)));

        // 设置为 Idle 状态
        info.status = Some(AgentStatus::Idle);

        // 超长超时时间（0秒）应该闲置
        assert!(info.is_fully_idle(Duration::from_secs(0)));
    }

    #[test]
    fn test_unified_container_info_is_fully_idle_computer() {
        let container = create_mock_container_basic_info("user_123");
        let info = UnifiedContainerInfo::new_computer("user_123".to_string(), container);

        // 没有项目，应该闲置
        assert!(info.is_fully_idle(Duration::from_secs(3600)));

        // 添加一个活跃项目
        let mut proj = ProjectInfo::new("proj_1".to_string());
        proj.status = Some(AgentStatus::Active);
        info.upsert_project("proj_1".to_string(), Arc::new(proj));

        // 有活跃项目，不闲置
        assert!(!info.is_fully_idle(Duration::from_secs(0)));

        // 将项目设置为 Idle
        let mut idle_proj = ProjectInfo::new("proj_1".to_string());
        idle_proj.status = Some(AgentStatus::Idle);
        info.upsert_project("proj_1".to_string(), Arc::new(idle_proj));

        // 所有项目都 Idle，且超时为 0，应该闲置
        assert!(info.is_fully_idle(Duration::from_secs(0)));
    }

    #[test]
    fn test_container_key_serialization() {
        let key = ContainerKey::from_project("proj_123".to_string());
        let serialized = serde_json::to_string(&key).unwrap();
        let deserialized: ContainerKey = serde_json::from_str(&serialized).unwrap();
        assert_eq!(key, deserialized);

        let key2 = ContainerKey::from_user("user_456".to_string());
        let serialized2 = serde_json::to_string(&key2).unwrap();
        let deserialized2: ContainerKey = serde_json::from_str(&serialized2).unwrap();
        assert_eq!(key2, deserialized2);
    }
}

//! DuckDB Manager 数据模型
//!
//! 定义数据库表对应的记录结构和辅助类型

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared_types::ServiceType;

/// 容器记录 - 对应 containers 表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    /// 容器ID (主键)
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 容器IP地址
    pub container_ip: String,
    /// 内部端口
    pub internal_port: u16,
    /// 外部端口
    pub external_port: u16,
    /// 服务类型
    pub service_type: ServiceType,
    /// 容器状态
    pub status: String,
    /// 服务URL
    pub service_url: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
}

impl ContainerRecord {
    /// 创建新的容器记录
    pub fn new(
        container_id: String,
        container_name: String,
        container_ip: String,
        internal_port: u16,
        external_port: u16,
        service_type: ServiceType,
        status: String,
        service_url: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            container_id,
            container_name,
            container_ip,
            internal_port,
            external_port,
            service_type,
            status,
            service_url,
            created_at: now,
            last_activity: now,
        }
    }

    /// 检查容器是否处于保护期（创建后 5 分钟内）
    pub fn is_in_protection_period(&self, protection_minutes: i64) -> bool {
        let elapsed = Utc::now().signed_duration_since(self.created_at);
        elapsed.num_minutes() < protection_minutes
    }

    /// 检查容器是否闲置
    pub fn is_idle(&self, idle_minutes: i64) -> bool {
        let elapsed = Utc::now().signed_duration_since(self.last_activity);
        elapsed.num_minutes() >= idle_minutes
    }
}

/// 项目记录 - 对应 projects 表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    /// 项目ID (主键)
    pub project_id: String,
    /// 会话ID
    pub session_id: Option<String>,
    /// 服务类型
    pub service_type: ServiceType,
    /// 关联的容器ID
    pub container_id: String,
    /// 用户ID (ComputerAgentRunner 模式)
    pub user_id: Option<String>,
    /// Agent 状态码
    pub agent_status_code: Option<i32>,
    /// Agent 状态名称
    pub agent_status_name: Option<String>,
    /// 请求ID
    pub request_id: Option<String>,
    /// 模型提供商配置 (JSON)
    pub model_provider_json: Option<String>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 会话创建时间
    pub session_created_at: Option<DateTime<Utc>>,
    /// 会话最后活动时间
    pub session_last_activity: Option<DateTime<Utc>>,
}

impl ProjectRecord {
    /// 创建新的项目记录
    pub fn new(
        project_id: String,
        service_type: ServiceType,
        container_id: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            session_id: None,
            service_type,
            container_id,
            user_id: None,
            agent_status_code: None,
            agent_status_name: None,
            request_id: None,
            model_provider_json: None,
            created_at: now,
            last_activity: now,
            session_created_at: None,
            session_last_activity: None,
        }
    }

    /// 创建带用户ID的项目记录 (ComputerAgentRunner 模式)
    pub fn new_with_user_id(
        project_id: String,
        user_id: String,
        service_type: ServiceType,
        container_id: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            session_id: None,
            service_type,
            container_id,
            user_id: Some(user_id),
            agent_status_code: None,
            agent_status_name: None,
            request_id: None,
            model_provider_json: None,
            created_at: now,
            last_activity: now,
            session_created_at: None,
            session_last_activity: None,
        }
    }

    /// 获取容器唯一标识
    ///
    /// 根据 service_type 返回不同的标识符：
    /// - RCoder 模式：返回 project_id
    /// - ComputerAgentRunner 模式：返回 user_id（如果存在）
    pub fn container_key(&self) -> &str {
        match self.service_type {
            ServiceType::ComputerAgentRunner => {
                self.user_id.as_deref().unwrap_or(&self.project_id)
            }
            ServiceType::RCoder => &self.project_id,
        }
    }
}

/// 清理结果统计
#[derive(Debug, Clone, Default)]
pub struct CleanupResult {
    /// 清理的容器数量
    pub cleaned_containers: usize,
    /// 清理的项目数量
    pub cleaned_projects: usize,
    /// 清理的孤立容器数量
    pub orphan_containers: usize,
    /// 清理的 gRPC 连接数量
    pub grpc_connections: usize,
    /// 清理的 VNC 后端数量
    pub vnc_backends: usize,
    /// 错误信息
    pub errors: Vec<String>,
}

impl CleanupResult {
    /// 创建新的清理结果
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加错误信息
    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }

    /// 合并另一个清理结果
    pub fn merge(&mut self, other: CleanupResult) {
        self.cleaned_containers += other.cleaned_containers;
        self.cleaned_projects += other.cleaned_projects;
        self.orphan_containers += other.orphan_containers;
        self.grpc_connections += other.grpc_connections;
        self.vnc_backends += other.vnc_backends;
        self.errors.extend(other.errors);
    }

    /// 是否有错误
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// 总清理数量
    pub fn total_cleaned(&self) -> usize {
        self.cleaned_containers + self.cleaned_projects + self.orphan_containers
    }
}

/// 存储统计信息
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    /// 总容器数量
    pub total_containers: usize,
    /// 总项目数量
    pub total_projects: usize,
    /// 活跃会话数量
    pub active_sessions: usize,
    /// 活跃容器数量
    pub active_containers: usize,
    /// 闲置容器数量
    pub idle_containers: usize,
    /// 按服务类型统计的项目数量
    pub projects_by_service_type: std::collections::HashMap<ServiceType, usize>,
}

impl StorageStats {
    /// 创建新的统计信息
    pub fn new() -> Self {
        Self::default()
    }
}

/// 闲置容器信息 - 用于清理任务
#[derive(Debug, Clone)]
pub struct IdleContainerInfo {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 服务类型
    pub service_type: ServiceType,
    /// 闲置时长（分钟）
    pub idle_minutes: i64,
    /// 关联的项目ID列表
    pub project_ids: Vec<String>,
}

/// 孤立容器信息 - 用于清理任务
#[derive(Debug, Clone)]
pub struct OrphanContainerInfo {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 服务类型
    pub service_type: ServiceType,
    /// 创建时间
    pub created_at: DateTime<Utc>,
}

//! 容器基本信息 DTO（自 agent_project_runner_model 拆出）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

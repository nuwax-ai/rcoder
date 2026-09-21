//! 容器基本信息 DTO（自 agent_project_runner_model 拆出）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 容器基本信息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// K8s workload 身份（STS/Deployment `metadata.uid`；注册表零包袱方案 §1.1）：
    /// 与 Pod UID（container_id）一同在创建/换代时捕获，对账按此判"同 workload"。
    /// Docker 无 workload 对象恒 None（容器名即 workload 身份）；Userapp 生产
    /// Deployment 的绑定身份在 userapp 生命周期表（physical_uid+deployment_generation），
    /// 此字段对其为 None。skip_serializing_if 保持既有序列化形状不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_uid: Option<String>,
}

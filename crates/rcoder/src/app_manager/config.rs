//! 应用管理服务配置

use serde::{Deserialize, Serialize};

/// 应用管理服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManagerConfig {
    /// 是否启用
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// 工作空间根目录（Docker 模式）
    #[serde(default = "default_workspace_root")]
    pub workspace_root: String,

    /// K8s 命名空间
    #[serde(default = "default_namespace")]
    pub namespace: String,

    /// K8s Gateway 名称
    #[serde(default = "default_gateway_name")]
    pub gateway_name: String,

    /// K8s Gateway 命名空间
    #[serde(default = "default_gateway_namespace")]
    pub gateway_namespace: String,

    /// K8s 节点 IP
    #[serde(default = "default_node_ip")]
    pub node_ip: String,

    /// K8s Gateway NodePort
    #[serde(default = "default_gateway_node_port")]
    pub gateway_node_port: u16,

    /// 存储类（K8s）
    #[serde(default = "default_storage_class")]
    pub storage_class: String,
}

impl Default for AppManagerConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            workspace_root: default_workspace_root(),
            namespace: default_namespace(),
            gateway_name: default_gateway_name(),
            gateway_namespace: default_gateway_namespace(),
            node_ip: default_node_ip(),
            gateway_node_port: default_gateway_node_port(),
            storage_class: default_storage_class(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_workspace_root() -> String {
    "./app-workspace".to_string()
}

fn default_namespace() -> String {
    "rcoder-apps".to_string()
}

fn default_gateway_name() -> String {
    "nuwax-gateway".to_string()
}

fn default_gateway_namespace() -> String {
    "default".to_string()
}

fn default_node_ip() -> String {
    "192.168.11.216".to_string()
}

fn default_gateway_node_port() -> u16 {
    30080
}

fn default_storage_class() -> String {
    "ceph-rbd".to_string()
}

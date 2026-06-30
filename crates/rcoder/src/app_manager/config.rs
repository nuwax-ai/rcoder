//! 应用管理服务配置

use serde::{Deserialize, Serialize};

/// 应用管理服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppManagerConfig {
    /// 是否启用
    pub enabled: bool,

    /// 工作空间根目录（Docker 模式，K8s 模式下可选）
    pub workspace_root: Option<String>,

    /// K8s 命名空间
    pub namespace: String,

    /// K8s Gateway 名称（可选，未配置时不使用 Gateway）
    pub gateway_name: Option<String>,

    /// K8s Gateway 命名空间（可选）
    pub gateway_namespace: Option<String>,

    /// K8s 节点 IP（可选，用于外部访问地址构建）
    pub node_ip: Option<String>,

    /// K8s Gateway NodePort（可选）
    pub gateway_node_port: Option<u16>,

    /// 存储类（K8s）
    pub storage_class: Option<String>,
}

impl Default for AppManagerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace_root: std::env::var("RCODER_WORKSPACE_ROOT").ok(),
            namespace: std::env::var("RCODER_K8S_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            gateway_name: std::env::var("RCODER_K8S_GATEWAY_NAME").ok(),
            gateway_namespace: std::env::var("RCODER_K8S_GATEWAY_NAMESPACE").ok(),
            node_ip: std::env::var("RCODER_K8S_NODE_IP").ok(),
            gateway_node_port: std::env::var("RCODER_K8S_GATEWAY_NODE_PORT")
                .ok()
                .and_then(|s| s.parse().ok()),
            storage_class: std::env::var("RCODER_K8S_STORAGE_CLASS").ok(),
        }
    }
}

impl AppManagerConfig {
    /// 获取工作空间根目录
    ///
    /// Docker 模式: 使用配置的 workspace_root
    /// K8s 模式: 如果未配置，使用默认的 K8s 工作空间路径
    pub fn get_workspace_root(&self) -> String {
        self.workspace_root.clone().unwrap_or_else(|| {
            // K8s 模式下，使用默认的工作空间路径（PVC 挂载点）
            "/app/app-workspace".to_string()
        })
    }

    /// 获取节点 IP
    pub fn get_node_ip(&self) -> String {
        self.node_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// 获取 Gateway 名称
    pub fn get_gateway_name(&self) -> String {
        self.gateway_name
            .clone()
            .unwrap_or_else(|| "nuwax-gateway".to_string())
    }

    /// 获取 Gateway 命名空间
    pub fn get_gateway_namespace(&self) -> String {
        self.gateway_namespace
            .clone()
            .unwrap_or_else(|| "default".to_string())
    }

    /// 获取 Gateway NodePort
    pub fn get_gateway_node_port(&self) -> u16 {
        self.gateway_node_port.unwrap_or(30080)
    }
}

//! 应用管理服务配置

use serde::{Deserialize, Serialize};

/// 应用对外暴露模式（决定 `build_access_info` 生成的访问地址格式）
///
/// - `Docker`：HTTP 经 Pingora `/proxy/{port}`，TCP 经 host_port（port_bindings）
/// - `Kubernetes`：HTTP 经 Gateway HTTPRoute `/apps/{app_id}`，TCP 经 NodePort
///
/// 默认由编译期 feature 决定（kubernetes → Kubernetes，否则 Docker），
/// 可被环境变量 `RCODER_APP_ACCESS_MODE` 覆盖，便于双后端运行时切换。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppAccessMode {
    Docker,
    Kubernetes,
}

impl Default for AppAccessMode {
    fn default() -> Self {
        // 运行时统一构造（plan G2）：默认值由 feature 给出，env 可覆盖。
        match std::env::var("RCODER_APP_ACCESS_MODE").ok().as_deref() {
            Some("docker") => AppAccessMode::Docker,
            Some("kubernetes") | Some("k8s") => AppAccessMode::Kubernetes,
            _ => {
                #[cfg(feature = "kubernetes")]
                {
                    AppAccessMode::Kubernetes
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    AppAccessMode::Docker
                }
            }
        }
    }
}

/// 应用管理服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppManagerConfig {
    /// 是否启用
    pub enabled: bool,

    /// 工作空间根目录（Docker 模式 = 宿主机路径；K8s 模式 = rcoder Pod 内 PVC 挂载点）
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

    /// 存储类（K8s，保留字段；当前 app 复用 rcoder-workspace PVC，不新建独立 PVC）
    pub storage_class: Option<String>,

    // ===== Layer 5 接线字段 =====
    /// 对外暴露模式（Docker / Kubernetes）
    pub access_mode: AppAccessMode,

    /// 对外主机地址（Docker 模式用于拼 Pingora / host_port 访问 URL；
    /// 未配置时回退到 node_ip 或 127.0.0.1）
    pub external_host: Option<String>,

    /// Pingora 监听端口（Docker 模式 HTTP 访问入口）
    pub pingora_listen_port: Option<u16>,

    /// 工作空间 PVC 名（K8s 模式，app 复用的 RWX PVC；运行时也直接读 env
    /// `RCODER_WORKSPACE_PVC_NAME`，此处仅作可观测/兜底）
    pub workspace_pvc_name: Option<String>,
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
            access_mode: AppAccessMode::default(),
            external_host: std::env::var("RCODER_EXTERNAL_HOST").ok(),
            pingora_listen_port: std::env::var("RCODER_PINGORA_LISTEN_PORT")
                .ok()
                .and_then(|s| s.parse().ok()),
            workspace_pvc_name: std::env::var("RCODER_WORKSPACE_PVC_NAME").ok(),
        }
    }
}

impl AppManagerConfig {
    /// 获取工作空间根目录
    ///
    /// Docker 模式: 使用配置的 workspace_root（宿主机路径）
    /// K8s 模式: 如果未配置，使用默认 `/app/project_workspace/apps`——匹配 rcoder Pod 的
    /// workspace 挂载点（rcoder-workspace PVC 的 `workspace` subPath → /app/project_workspace），
    /// 其下 `{app_id}` 子目录与 app Pod（subPath `workspace/apps/{app_id}` → /app）共享同一 PVC 物理路径。
    pub fn get_workspace_root(&self) -> String {
        self.workspace_root
            .clone()
            .unwrap_or_else(|| "/app/project_workspace/apps".to_string())
    }

    /// 获取节点 IP
    pub fn get_node_ip(&self) -> String {
        self.node_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// 获取对外主机地址（Docker 模式优先；回退到 node_ip）
    pub fn get_external_host(&self) -> String {
        self.external_host
            .clone()
            .or_else(|| self.node_ip.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// 获取 Pingora 监听端口（Docker 模式 HTTP 入口）
    pub fn get_pingora_listen_port(&self) -> u16 {
        self.pingora_listen_port.unwrap_or(8088)
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

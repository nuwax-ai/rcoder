use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

mod storage;
pub use storage::{StorageBackend, StorageConfig};

/// 命令行参数
#[derive(Parser, Debug)]
#[command(name = "rcoder")]
#[command(about = "RCoder - Rust-based AI Agent Framework")]
#[command(version)]
pub struct CliArgs {
    /// 主服务端口
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// 项目工作目录
    #[arg(short = 'd', long, default_value = "./project_workspace")]
    pub projects_dir: Option<String>,

    /// 启用反向代理
    #[arg(short, long)]
    pub enable_proxy: bool,

    /// 代理服务端口
    #[arg(long = "proxy-port")]
    pub proxy_port: Option<u16>,

    /// 默认后端服务端口
    #[arg(long = "backend-port")]
    pub default_backend_port: Option<u16>,

    /// 管理子命令 (不传 = 正常启动服务)
    #[command(subcommand)]
    pub command: Option<AdminCommand>,
}

/// 管理子命令 (操作运行中的 rcoder 进程, 经 localhost HTTP)。
#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// 管理 60000 file-server 分流反向代理 (开发测试期 TS↔分流代理切换对比)
    FileServer {
        #[command(subcommand)]
        action: FileServerAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum FileServerAction {
    /// 启动分流反向代理 (幂等; 端口取 config.yml file_server_proxy 段, 默认 60000)
    Start,
    /// 停止并释放 60000 端口 (幂等; 10s 超时强制) —— 释放后可起 TS nuwax-file-server 直跑对比
    Stop,
    /// 停止后重新启动
    Restart,
    /// 查看运行状态
    Status,
}

// 从 shared_types 导入 API Key 鉴权配置
pub use shared_types::ApiKeyAuthConfig;

/// 应用程序配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 默认使用的 Agent ID
    #[serde(default = "default_agent_id", alias = "default_agent")]
    pub default_agent_id: String,
    /// 项目工作目录
    pub projects_dir: PathBuf,
    /// 主服务端口
    pub port: u16,
    /// 反向代理配置
    pub proxy_config: Option<ProxyConfig>,
    /// file-server 分流反向代理（60000 入口：userApp → 主服务, 其余 → TS nuwax-file-server）
    ///
    /// 段缺失 → None → 不监听 60000（本地 dev 形态）；K8s 部署经 helm 渲染此段。
    #[serde(default)]
    pub file_server_proxy: Option<file_server_proxy::FileServerProxyConfig>,
    /// Docker 配置(docker 运行时读,K8s 不读)
    pub docker_config: Option<DockerConfig>,
    /// K8s 运行时配置(K8s 运行时读,docker 不读;与 docker_config 完全分家)
    ///
    /// docker 部署下此键缺失 → None;AppConfig 无 deny_unknown_fields,
    /// 故 docker 部署即使误带 kubernetes_config 键也不会报错(被忽略)。
    #[serde(default)]
    pub kubernetes_config: Option<shared_types::KubernetesConfig>,
    /// 容器清理配置
    #[serde(default)]
    pub cleanup_config: CleanupConfigSettings,
    /// UserApp 闲置自动回收 + 流量唤醒配置
    #[serde(default)]
    pub userapp_recycle: UserAppRecycleConfig,
    /// 存储后端配置（rcoder-pg：memory=纯内存单节点，postgres=PG 持久化）
    #[serde(default)]
    pub storage: StorageConfig,
    /// API Key 鉴权配置
    #[serde(default)]
    pub api_key_auth: ApiKeyAuthConfig,
    /// 应用管理配置
    #[serde(default)]
    pub app_manager: app_manager::AppManagerConfig,
}

pub(crate) fn default_agent_id() -> String {
    shared_types::DEFAULT_AGENT_ID.to_string()
}

/// 生成随机 API Key
/// 使用 UUID v4 生成随机密钥，格式：sk-{uuid}
pub(crate) fn generate_random_api_key() -> String {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    format!("sk-{}", uuid.simple())
}

pub const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent_id: default_agent_id(),
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8087,
            proxy_config: Some(ProxyConfig::default()),
            file_server_proxy: None,
            docker_config: Some(DockerConfig::default()),
            kubernetes_config: None,
            cleanup_config: CleanupConfigSettings::default(),
            userapp_recycle: UserAppRecycleConfig::default(),
            storage: StorageConfig::default(),
            api_key_auth: ApiKeyAuthConfig {
                enabled: false,
                api_key: generate_random_api_key(),
            },
            app_manager: app_manager::AppManagerConfig::default(),
        }
    }
}

mod loader;
mod sections;

pub use loader::{load_api_key_config_from_file, load_config_with_args};
pub use sections::{CleanupConfigSettings, DockerConfig, ProxyConfig, UserAppRecycleConfig};

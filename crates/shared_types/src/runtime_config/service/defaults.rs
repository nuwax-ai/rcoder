//! 服务配置默认值（serde default 函数 + rcoder/agent-runner 两份默认配置）。

use std::collections::HashMap;

use super::image::ServiceImageConfig;
use super::resource::ServiceResourceLimits;
use crate::ServiceType;

/// 容器路径模板的默认值
pub(super) fn default_container_path_template() -> String {
    std::path::PathBuf::from(crate::paths::WORKSPACE_ROOT)
        .join("{project_id}")
        .to_string_lossy()
        .into_owned()
}

/// Computer Agent Runner 容器路径模板的默认值
pub(super) fn default_computer_agent_runner_container_path_template() -> String {
    std::path::PathBuf::from(crate::paths::COMPUTER_WORKSPACE_ROOT)
        .join("{user_id}")
        .join("{project_id}")
        .to_string_lossy()
        .into_owned()
}

/// 容器工作目录的默认值
pub(super) fn default_work_dir() -> String {
    "/app".to_string()
}

/// 容器网络模式的默认值
pub(super) fn default_network_mode() -> String {
    "bridge".to_string()
}

/// 创建默认的 RCoder 服务配置
pub fn default_rcoder_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "info".to_string());
    environment.insert("SERVICE_MODE".to_string(), "full".to_string());
    environment.insert("API_PORT".to_string(), "8086".to_string());

    // 🔥 默认不提供挂载配置，让配置文件控制
    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制
    let resource_limits = ServiceResourceLimits::new(
        Some(2_000_000_000.0), // 2GB
        Some(2.0),             // 2 核
        Some(4_000_000_000.0), // 4GB
        None, // storage_size: 由 k8s_pvc.rs DEFAULT_PVC_STORAGE_SIZE 兜底(当前 10Gi)
        None, // ephemeral_storage_limit: 回退到 storage_size
    );

    ServiceImageConfig {
        service_type: ServiceType::WebAgentRunner,
        image: None,         // 使用架构特定镜像
        arm64_image: None,   // 从配置文件加载
        amd64_image: None,   // 从配置文件加载
        default_image: None, // 从配置文件加载
        image_tag_prefix: Some("web-agent-runner".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_container_path_template(),
        workspace_resolution_path: None,
        security: None,
    }
}

/// 创建默认的 Computer Agent Runner 服务配置
pub fn default_agent_runner_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "debug".to_string());
    environment.insert("SERVICE_MODE".to_string(), "agent-only".to_string());
    environment.insert("AGENT_PORT".to_string(), "8086".to_string());
    environment.insert(
        "PROJECT_WORKSPACE_BASE".to_string(),
        "/home/user".to_string(),
    );

    // 🔥 Agent 清理配置（通过环境变量控制）
    // 设置为 3600 秒（1小时），用户可以在 docker/config.yml 中覆盖此值
    environment.insert(
        "RCODER_AGENT_IDLE_TIMEOUT_SECS".to_string(),
        "3600".to_string(),
    ); // 1 小时

    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制（ComputerAgentRunner 可能需要更多资源）
    let resource_limits = ServiceResourceLimits::new(
        Some(4_000_000_000.0), // 4GB
        Some(3.0),             // 3 核
        Some(8_000_000_000.0), // 8GB
        None, // storage_size: 由 k8s_pvc.rs DEFAULT_PVC_STORAGE_SIZE 兜底(当前 10Gi)
        None, // ephemeral_storage_limit: 回退到 storage_size
    );

    ServiceImageConfig {
        service_type: ServiceType::ComputerAgentRunner,
        image: None,         // 使用架构特定镜像
        arm64_image: None,   // 从配置文件加载
        amd64_image: None,   // 从配置文件加载
        default_image: None, // 从配置文件加载
        image_tag_prefix: Some("computer-agent-runner".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_computer_agent_runner_container_path_template(),
        workspace_resolution_path: None,
        security: None,
    }
}

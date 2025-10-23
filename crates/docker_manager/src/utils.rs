use super::{DockerContainerConfig, DockerManagerConfig, DockerResult};
use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

/// Docker 工具函数
pub struct DockerUtils;

impl DockerUtils {
    /// 自动检测当前系统架构并返回对应的 Docker 平台字符串
    pub fn auto_detect_platform() -> String {
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;

        debug!("检测到系统架构: {} {}", os, arch);

        match (os, arch) {
            // macOS ARM64
            ("macos", "aarch64") => "linux/arm64",
            // Linux ARM64
            ("linux", "aarch64") => "linux/arm64",
            // macOS AMD64
            ("macos", "x86_64") => "linux/amd64",
            // Linux AMD64
            ("linux", "x86_64") => "linux/amd64",
            // Windows AMD64 (在 Windows 上运行 Docker Desktop)
            ("windows", "x86_64") => "linux/amd64",
            // 其他 ARM64 变体
            (_, "arm64") => "linux/arm64",
            // 默认回退到 AMD64
            _ => {
                debug!("未知架构 {} {}, 默认使用 linux/amd64", os, arch);
                "linux/amd64"
            }
        }
        .to_string()
    }

    /// 获取最佳的平台配置（优先使用环境变量，否则自动检测）
    pub fn get_optimal_platform() -> String {
        // 优先使用环境变量
        if let Ok(platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
            debug!("使用环境变量配置的平台: {}", platform);
            return platform;
        }

        // 否则自动检测
        let detected = Self::auto_detect_platform();
        debug!("自动检测到的平台: {}", detected);
        detected
    }

    /// 检查镜像是否与当前架构兼容
    pub fn is_image_compatible_with_current_arch(image_tag: &str) -> bool {
        let current_platform = Self::get_optimal_platform();

        // 根据镜像标签判断架构
        let image_platform = if image_tag.contains("arm64") || image_tag.contains("aarch64") {
            "linux/arm64"
        } else if image_tag.contains("amd64") || image_tag.contains("x86_64") {
            "linux/amd64"
        } else {
            // 如果标签不包含架构信息，假设兼容
            return true;
        };

        let compatible = current_platform == image_platform;
        debug!(
            "镜像 {} 平台 {} 与当前平台 {} 兼容性: {}",
            image_tag, image_platform, current_platform, compatible
        );
        compatible
    }

    /// 根据项目 ID 创建容器配置
    pub fn create_config_from_project_id(
        project_id: &str,
        base_workspace_dir: &str,
        custom_image: Option<String>,
    ) -> DockerContainerConfig {
        let project_path = format!("{}/{}", base_workspace_dir, project_id);

        let mut config = DockerContainerConfig::default();
        config.project_id = project_id.to_string();
        config.host_path = project_path.clone();

        if let Some(image) = custom_image {
            config.image = image;
        }

        // 设置默认环境变量
        config
            .env_vars
            .insert("RUST_LOG".to_string(), "info".to_string());
        config
            .env_vars
            .insert("TZ".to_string(), "Asia/Shanghai".to_string());

        debug!(
            "为项目 {} 创建 Docker 配置: workspace={}",
            project_id, project_path
        );

        config
    }

    /// 为 RCoder Agent 创建标准配置
    pub fn create_rcoder_agent_config(
        project_id: &str,
        base_workspace_dir: &str,
        agent_type: &str,
        additional_env: HashMap<String, String>,
    ) -> DockerContainerConfig {
        let mut config = Self::create_config_from_project_id(project_id, base_workspace_dir, None);

        // 添加 Agent 类型环境变量
        config
            .env_vars
            .insert("AGENT_TYPE".to_string(), agent_type.to_string());

        // 添加额外环境变量
        for (key, value) in additional_env {
            config.env_vars.insert(key, value);
        }

        // 根据需要设置端口映射
        if agent_type == "claude" {
            // Claude Code 可能需要特定端口
            config
                .port_bindings
                .insert("8086/tcp".to_string(), "0".to_string()); // 动态分配端口
        }

        config
    }

    /// 验证项目路径是否存在
    pub fn validate_project_path(project_path: &str) -> DockerResult<()> {
        if !Path::new(project_path).exists() {
            return Err(super::DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("项目路径不存在: {}", project_path),
            )));
        }
        Ok(())
    }

    /// 规范化项目路径
    pub fn normalize_project_path(project_path: &str) -> String {
        let path = Path::new(project_path);

        // 如果是相对路径，转换为绝对路径
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap().join(path)
        };

        // 规范化路径
        absolute_path
            .canonicalize()
            .unwrap_or_else(|_| absolute_path)
            .to_string_lossy()
            .to_string()
    }

    /// 生成容器名称：使用 project_id 而不是随机 UUID，便于管理和调试
    pub fn generate_container_name(prefix: &str, project_id: &str) -> String {
        format!("{}-{}", prefix, project_id)
    }

    /// 从环境变量加载 Docker 配置
    pub fn config_from_env() -> DockerManagerConfig {
        let mut config = DockerManagerConfig::default();

        if let Ok(host) = std::env::var("DOCKER_HOST") {
            config.docker_host = Some(host);
        }

        if let Ok(image) = std::env::var("DEFAULT_DOCKER_IMAGE") {
            config.default_image = image;
        }

        // 自动检测平台配置（优先使用环境变量，否则自动检测）
        config.default_platform = Self::get_optimal_platform();

        // 🔍 调试日志：打印自动检测结果
        debug!("🔍 DockerUtils::config_from_env 自动检测结果:");
        debug!("  - default_platform: {}", config.default_platform);
        debug!("  - default_image: {}", config.default_image);

        if let Ok(network) = std::env::var("DOCKER_NETWORK_MODE") {
            config.default_network_mode = network;
        }

        if let Ok(work_dir) = std::env::var("DOCKER_WORK_DIR") {
            config.default_work_dir = work_dir;
        }

        if let Ok(auto_cleanup) = std::env::var("DOCKER_AUTO_CLEANUP") {
            config.auto_cleanup = auto_cleanup.parse().unwrap_or(true);
        }

        if let Ok(ttl) = std::env::var("DOCKER_CONTAINER_TTL") {
            config.container_ttl_seconds = ttl.parse().ok();
        }

        config
    }
}

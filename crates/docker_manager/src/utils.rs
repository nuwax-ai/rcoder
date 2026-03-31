use super::DockerManagerConfig;
use tracing::debug;

/// Docker 工具函数
pub struct DockerUtils;

impl DockerUtils {
    /// 自动检测当前系统架构并返回对应的 Docker 平台字符串
    pub fn auto_detect_platform() -> String {
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;

        debug!("detect message : {} {}", os, arch);

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
                debug!("not message {} {}, default message linux/amd64", os, arch);
                "linux/amd64"
            }
        }
        .to_string()
    }

    /// 获取最佳的平台配置（优先使用环境变量，否则自动检测）
    pub fn get_optimal_platform() -> String {
        // 优先使用环境变量
        if let Ok(platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
            debug!(" message config message : {}", platform);
            return platform;
        }

        // 否则自动检测
        let detected = Self::auto_detect_platform();
        debug!(" message detect message : {}", detected);
        detected
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
        debug!("DockerUtils::config_from_env message detect message :");
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
            match ttl.parse() {
                Ok(seconds) => config.container_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [DOCKER] 无法解析 DOCKER_CONTAINER_TTL '{}': {}, 使用默认值",
                        ttl,
                        e
                    );
                }
            }
        }

        if let Ok(network_base_name) = std::env::var("DOCKER_NETWORK_BASE_NAME") {
            config.network_base_name = network_base_name;
        }

        config
    }
}

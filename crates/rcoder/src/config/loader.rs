//! 配置加载器（从 config/mod.rs 拆出）。
//!
//! `load_config_with_args`（CLI + config.yml + env 覆盖）、`load_config` 及
//! 默认配置文件生成；CONFIG_FILE 常量在 [`super`]。

use std::fs;
use std::path::PathBuf;

use tracing::{info, warn};

use super::sections::*;
use super::sections::{env_override_bool, env_override_u64};
use super::storage;
use super::{ApiKeyAuthConfig, AppConfig, CONFIG_FILE, CliArgs, generate_random_api_key};

pub fn load_config_with_args(cli_args: CliArgs) -> anyhow::Result<AppConfig> {
    let mut config = if std::path::Path::new(CONFIG_FILE).exists() {
        // 尝试从文件加载配置
        match load_config_from_file() {
            Ok(file_config) => {
                info!("Config file already loaded: {}", CONFIG_FILE);
                file_config
            }
            Err(e) => {
                warn!("Failed to load config file, using default config: {}", e);
                AppConfig::default()
            }
        }
    } else {
        info!(
            "config file not found, created default config file: {}",
            CONFIG_FILE
        );
        let default_config = AppConfig::default();
        create_default_config_file(&default_config)?;
        default_config
    };

    // 命令行参数覆盖配置文件
    if let Some(port) = cli_args.port {
        config.port = port;
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    // 环境变量覆盖所有配置
    if let Ok(port) = std::env::var("RCODER_PORT") {
        if let Ok(port) = port.parse::<u16>() {
            config.port = port;
        } else {
            warn!(" parse RCODER_PORT failed: {}", port);
        }
    }

    if let Ok(projects_dir) = std::env::var("RCODER_PROJECTS_DIR") {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    // 如果启用了代理，配置代理相关参数
    if cli_args.enable_proxy {
        let mut proxy_config = ProxyConfig::default();

        if let Some(proxy_port) = cli_args.proxy_port {
            proxy_config.listen_port = proxy_port;
        }

        if let Some(default_backend_port) = cli_args.default_backend_port {
            proxy_config.default_backend_port = default_backend_port;
        }

        config.proxy_config = Some(proxy_config);
    }

    // 应用 Docker 配置的环境变量覆盖
    if let Some(docker_config) = &mut config.docker_config {
        docker_config.apply_env_overrides()?;
    }

    // 应用 API Key 配置的环境变量覆盖
    if let Ok(val) = std::env::var("RCODER_API_KEY_ENABLED") {
        if let Ok(enabled) = val.parse::<bool>() {
            config.api_key_auth.enabled = enabled;
            info!(" RCODER_API_KEY_ENABLED: {}", enabled);
        } else {
            warn!(" parse RCODER_API_KEY_ENABLED failed: {}", val);
        }
    }

    if let Ok(val) = std::env::var("RCODER_API_KEY") {
        config.api_key_auth.api_key = val.clone();
        info!(" RCODER_API_KEY configured");
    }

    // 应用 UserApp 自动回收配置的环境变量覆盖
    env_override_bool(
        "RCODER_USERAPP_RECYCLE_ENABLED",
        &mut config.userapp_recycle.enabled,
    );
    env_override_u64(
        "RCODER_USERAPP_IDLE_TIMEOUT_SECONDS",
        &mut config.userapp_recycle.idle_timeout_seconds,
    );
    env_override_u64(
        "RCODER_USERAPP_SCAN_INTERVAL_SECONDS",
        &mut config.userapp_recycle.scan_interval_seconds,
    );
    env_override_u64(
        "RCODER_USERAPP_WAKE_TIMEOUT_SECONDS",
        &mut config.userapp_recycle.wake_timeout_seconds,
    );
    env_override_u64(
        "RCODER_USERAPP_PROTECTION_SECONDS",
        &mut config.userapp_recycle.protection_seconds,
    );

    storage::apply_storage_env_overrides(&mut config)?;

    // 验证 API Key 配置
    if config.api_key_auth.enabled && config.api_key_auth.api_key.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "API Key authentication is enabled but API Key is empty, please check config file or environment variables"
        ));
    }

    // 配置验证
    if let Some(docker_config) = &config.docker_config
        && let Err(e) = docker_config.validate_multi_image_config()
    {
        return Err(anyhow::anyhow!(
            "Docker configuration validation failed: {}",
            e
        ));
    }

    info!(
        "Final config: port={}, projects_dir={:?}, default_agent_id={}, proxy_enabled={}",
        config.port,
        config.projects_dir,
        config.default_agent_id,
        config.proxy_config.is_some()
    );

    Ok(config)
}


/// 从文件加载配置
fn load_config_from_file() -> anyhow::Result<AppConfig> {
    let config_content = fs::read_to_string(CONFIG_FILE)
        .map_err(|e| anyhow::anyhow!("Failed to read config file: {}", e))?;

    // 安全修复：移除完整配置内容的 debug 日志，避免泄露 API Key 等敏感信息
    tracing::debug!("config file loaded, size: {} bytes", config_content.len());

    let config: AppConfig = serde_yaml::from_str(&config_content).map_err(|e| {
        tracing::error!("[CONFIG] Failed to parse config file: {}", e);
        // 打印配置文件的前 2000 个字符，帮助排查解析错误
        let preview = if config_content.len() > 2000 {
            format!("{}...(truncated)", &config_content[..2000])
        } else {
            config_content.clone()
        };
        tracing::error!("[CONFIG] Config file content preview:\n{}", preview);
        anyhow::anyhow!("Failed to parse config file: {}", e)
    })?;

    // 调试：打印解析后的多镜像配置
    if let Some(ref docker_config) = config.docker_config {
        tracing::info!("[CONFIG] docker_config is Some, checking multi_image_config");
        if let Some(ref multi_config) = docker_config.multi_image_config {
            tracing::info!(
                "[CONFIG] multi_image_config is Some, services count: {}",
                multi_config.services.len()
            );
            for (service_key, service_config) in &multi_config.services {
                tracing::info!(
                    "[CONFIG]   Service '{}': service_type={}, image={:?}, arm64_image={:?}, amd64_image={:?}, default_image={:?}, enabled={}",
                    service_key,
                    service_config.service_type,
                    service_config.image,
                    service_config.arm64_image,
                    service_config.amd64_image,
                    service_config.default_image,
                    service_config.enabled
                );
                tracing::debug!(
                    "  Service '{}' mount config (total {} mounts):",
                    service_key,
                    service_config.mounts.len()
                );
                for (i, mount) in service_config.mounts.iter().enumerate() {
                    tracing::debug!(
                        "    [{}]: {} -> {} ({})",
                        i,
                        mount.container_path,
                        mount.host_path,
                        mount.mount_type
                    );
                }
            }
        }
    }

    Ok(config)
}

/// 从配置文件中仅加载 API Key 配置（用于热更新）
///
/// 此函数由 config_watcher 模块调用,用于配置热重载。
/// 编译器可能误报为未使用,因为是跨模块调用。
#[allow(dead_code)]
pub fn load_api_key_config_from_file(
    config_path: &std::path::Path,
) -> anyhow::Result<ApiKeyAuthConfig> {
    let config_content = fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file: {}", e))?;

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file: {}", e))?;

    Ok(config.api_key_auth)
}

/// 创建默认配置文件
fn create_default_config_file(_config: &AppConfig) -> anyhow::Result<()> {
    // 检查配置文件是否已存在
    if std::path::Path::new(CONFIG_FILE).exists() {
        return Ok(());
    }

    // 创建配置文件目录（如果不存在）
    if let Some(parent) = std::path::Path::new(CONFIG_FILE).parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
    }

    // 使用嵌入式配置文件
    let default_config = include_str!("rcoder_default.yml");

    // 🆕 生成随机 API Key 并替换模板占位符
    let generated_api_key = generate_random_api_key();
    let config_content = default_config.replace("{{GENERATED_API_KEY}}", &generated_api_key);

    fs::write(CONFIG_FILE, config_content)
        .map_err(|e| anyhow::anyhow!("Failed to write default config file: {}", e))?;

    info!("Created default config file: {}", CONFIG_FILE);
    info!(" Loaded API Key (not set)");
    Ok(())
}

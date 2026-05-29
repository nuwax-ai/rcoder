//! Docker/K8s 运行时初始化与启动时容器清理

use docker_manager::container_stop;
use docker_manager::runtime_selection::RuntimeType;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::utils;

pub async fn init_path_resolver(runtime_type: RuntimeType) -> anyhow::Result<()> {
    if runtime_type == RuntimeType::Kubernetes {
        info!("[K8S] Kubernetes runtime mode, skipping Docker socket path resolver");
        return Ok(());
    }

    let docker_socket_path = std::env::var("DOCKER_SOCKET_PATH").unwrap_or_else(|_| {
        info!("DOCKER_SOCKET_PATH not set, using default: /var/run/docker.sock");
        "/var/run/docker.sock".to_string()
    });

    info!("Docker socket: {}", docker_socket_path);

    let _path_resolver =
        match utils::HostPathResolver::new_with_docker_socket(Some(docker_socket_path.clone()))
            .await
        {
            Ok(resolver) => {
                info!("path resolver initialized successfully");
                info!(
                    "  Container workspace: {:?}",
                    resolver.container_workspace_base()
                );
                info!("work directory: {:?}", resolver.host_workspace_base());
                Some(resolver)
            }
            Err(e) => {
                error!("path resolver initialization failed: {}", e);
                error!("please check config:");
                error!("1. Docker socket path: {}", docker_socket_path);
                error!("2. Docker socket already mounted in container");
                error!("3. container has Docker API access");
                error!("4. project work directory mounted");

                show_docker_configuration_help(&docker_socket_path);

                return Err(anyhow::anyhow!(
                    "Container self-check failed, unable to initialize path resolver"
                ));
            }
        };

    Ok(())
}

pub fn build_docker_manager_config(config: &AppConfig) -> docker_manager::DockerManagerConfig {
    if let Some(docker_config) = &config.docker_config {
        info!("using Docker config, merging config");
        let mut default_config = docker_manager::DockerManagerConfig::default();

        let app_multi_config = docker_config.get_multi_image_config();
        default_config.multi_image_config = app_multi_config;

        default_config.auto_cleanup = docker_config
            .auto_cleanup
            .unwrap_or(default_config.auto_cleanup);

        if let Some(ttl) = docker_config.container_ttl_seconds {
            default_config.container_ttl_seconds = Some(ttl);
        }

        info!(
            "🔍 [DEBUG] docker_config.network_base_name = {:?}",
            docker_config.network_base_name
        );
        if let Some(ref network_base_name) = docker_config.network_base_name {
            info!("using config: {}", network_base_name);
            default_config.network_base_name = network_base_name.clone();
        } else {
            info!(
                "⚠️ No network_base_name in config, using default: {}",
                default_config.network_base_name
            );
        }

        if let Some(timeout) = docker_config.api_timeout_seconds {
            default_config.api_timeout_seconds = timeout;
            info!("using config: API timeout: {} seconds", timeout);
        }
        if let Some(timeout) = docker_config.api_timeout_quick_seconds {
            default_config.api_timeout_quick_seconds = timeout;
            info!("using config: timeout: {} seconds", timeout);
        }

        if let Some(ttl) = docker_config.cache_status_ttl_seconds {
            default_config.cache_status_ttl_seconds = ttl;
            info!("using config: status cache TTL: {} seconds", ttl);
        }
        if let Some(ttl) = docker_config.cache_network_ttl_seconds {
            default_config.cache_network_ttl_seconds = ttl;
            info!("using config: network cache TTL: {} seconds", ttl);
        }
        if let Some(capacity) = docker_config.cache_max_capacity {
            default_config.cache_max_capacity = capacity;
            info!("using config: cache max capacity: {}", capacity);
        }

        default_config
    } else {
        info!("⚠️ no Docker config, using default config");
        docker_manager::DockerManagerConfig::default()
    }
}

pub async fn init_docker_manager(config: &AppConfig) -> anyhow::Result<()> {
    info!("initialize Docker Manager (with config)...");

    let docker_manager_config = build_docker_manager_config(config);

    if let Err(e) =
        docker_manager::global::init_global_docker_manager_with_config(docker_manager_config).await
    {
        error!("Docker Manager initialization failed: {}", e);
        return Err(anyhow::anyhow!(
            "Docker Manager initialization failed: {}",
            e
        ));
    }

    Ok(())
}

pub async fn startup_cleanup(config: &AppConfig) {
    info!("checking cleanup for container (enabled)...");
    if !config.cleanup_config.enabled {
        info!("Container cleanup task already started (cleanup_config.enabled=false)");
        return;
    }

    match docker_manager::runtime::RuntimeManager::runtime_type() {
        RuntimeType::Docker => {
            let docker_manager = match docker_manager::global::get_global_docker_manager().await {
                Ok(dm) => {
                    info!("Docker Manager initialized successfully (with config)");
                    dm
                }
                Err(e) => {
                    error!("get Docker Manager failed: {}", e);
                    return;
                }
            };

            let multi_image_config = if let Some(docker_config) = &config.docker_config {
                docker_config.get_multi_image_config()
            } else {
                shared_types::create_default_multi_image_config()
            };

            match container_stop::startup_cleanup_all_enabled_services(
                &docker_manager,
                &multi_image_config,
            )
            .await
            {
                Ok(result) => {
                    let enabled_services = shared_types::get_enabled_service_types(&multi_image_config);
                    if result.successfully_removed > 0 {
                        info!(
                            "✅ Startup cleanup completed, removed {} leftover containers (covering {} service types)",
                            result.successfully_removed,
                            enabled_services.len()
                        );
                    } else {
                        info!("no containers to cleanup");
                    }

                    if result.failed_removals > 0 {
                        warn!(
                            "container cleanup failed: failed count={}",
                            result.failed_removals
                        );
                        for failure in &result.failed_removals_details {
                            warn!(
                                "  - Container {} ({}): {}",
                                failure.container_id,
                                failure.container_name,
                                failure.error_message
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!("container cleanup failed: {}, cleanup skipped", e);
                }
            }
        }
        RuntimeType::Kubernetes => match docker_manager::runtime::RuntimeManager::get().await {
            Ok(runtime) => {
                if let Err(e) = runtime.cleanup_all().await {
                    warn!("k8s startup cleanup failed: {}", e);
                } else {
                    info!("k8s startup cleanup completed");
                }
            }
            Err(e) => warn!("failed to get runtime for k8s startup cleanup: {}", e),
        },
    }
}

pub fn get_container_prefixes(config: &AppConfig) -> anyhow::Result<(String, String)> {
    let docker_config = config
        .docker_config
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Docker config is required for container prefix"))?;
    let multi_config = docker_config.get_multi_image_config();
    let selector = docker_manager::image_selector::ImageSelector::new(multi_config);

    let (container_prefix_rcoder, container_prefix_computer) =
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rcoder_prefix = selector
                    .get_service_config(&shared_types::ServiceType::RCoder)
                    .await
                    .expect("Failed to get RCoder service config")
                    .container_prefix()
                    .to_string();
                let computer_prefix = selector
                    .get_service_config(&shared_types::ServiceType::ComputerAgentRunner)
                    .await
                    .expect("Failed to get ComputerAgentRunner service config")
                    .container_prefix()
                    .to_string();
                (rcoder_prefix, computer_prefix)
            })
        });

    Ok((container_prefix_rcoder, container_prefix_computer))
}

fn show_docker_configuration_help(socket_path: &str) {
    error!("📋 Docker config help:");
    error!("");
    error!("add to docker-compose.yml config:");
    error!("");
    error!("services:");
    error!("  rcoder:");
    error!("    environment:");
    error!("      - DOCKER_SOCKET_PATH={}", socket_path);
    error!("    volumes:");
    error!("      - {}:/var/run/docker.sock:ro", socket_path);
    error!("      - ./data/rcoder/project_workspace:/app/project_workspace");
    error!("");
    error!("🔧 Docker socket path:");
    error!(" Linux: /var/run/docker.sock");
    error!("  macOS + Docker Desktop: /var/run/docker.sock");
    error!("  Rootless Docker: /run/user/$UID/docker.sock");
    error!("");
    error!("🛠️ troubleshooting:");
    error!("1. check Docker: docker ps");
    error!("2. check socket file exists: ls -l {}", socket_path);
    error!("3. check docker group: groups $USER | grep docker");
    error!(
        "  4. Test Docker API: curl --unix-socket {} http://localhost/info",
        socket_path
    );
    error!("");
    error!("socket exists, rcoder container may not have access");
}

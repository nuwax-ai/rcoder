//! Docker/K8s 运行时初始化与启动时容器清理

use docker_manager::container_stop;
use docker_manager::runtime_selection::RuntimeType;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::utils;

pub async fn init_path_resolver(runtime_type: RuntimeType) -> anyhow::Result<()> {
    // deploy-host 宿主机形态：不做容器自检（宿主机无 /proc/self/cgroup），
    // 路径解析改"容器根→宿主机根"映射（默认 ~/.rcoder 约定，env 覆盖）。
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        let resolver = docker_manager::path::HostPathResolver::new_host_mode()?;
        info!(
            "[deploy-host] host path map ({} entries):",
            resolver.snapshot_map().len()
        );
        for (container_root, host_root) in resolver.snapshot_map() {
            info!(
                "[deploy-host]   {} -> {}",
                container_root.display(),
                host_root.display()
            );
        }
        rehydrate_deploy_host_docker_ports().await;
        return Ok(());
    }

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

        // K8s 运行时专用配置(docker 模式不读;K8s 模式从 config.yml 的 kubernetes_config 段取)
        default_config.kubernetes_config = config.kubernetes_config.clone().unwrap_or_default();
        if !default_config.kubernetes_config.services.is_empty() {
            info!(
                "[K8S] loaded kubernetes_config from config.yml: {} service(s), registry_prefix={:?}",
                default_config.kubernetes_config.services.len(),
                default_config
                    .kubernetes_config
                    .global_defaults
                    .registry_prefix
            );
        }

        default_config.auto_cleanup = docker_config
            .auto_cleanup
            .unwrap_or(default_config.auto_cleanup);

        if let Some(ttl) = docker_config.container_ttl_seconds {
            default_config.container_ttl_seconds = Some(ttl);
        }

        info!(
            "[DEBUG] docker_config.network_base_name = {:?}",
            docker_config.network_base_name
        );
        if let Some(ref network_base_name) = docker_config.network_base_name {
            info!("using config: {}", network_base_name);
            default_config.network_base_name = network_base_name.clone();
        } else {
            info!(
                " No network_base_name in config, using default: {}",
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
        info!(" no Docker config, using default config");
        // 即使无 docker_config,K8s 模式仍可能独立提供 kubernetes_config
        let k8s_cfg = config.kubernetes_config.clone().unwrap_or_default();
        if !k8s_cfg.services.is_empty() {
            info!(
                "[K8S] loaded kubernetes_config (no docker_config branch): {} service(s)",
                k8s_cfg.services.len()
            );
        }
        docker_manager::DockerManagerConfig {
            kubernetes_config: k8s_cfg,
            ..Default::default()
        }
    }
}

pub async fn init_docker_manager(config: &AppConfig) -> anyhow::Result<()> {
    info!("initialize Docker Manager (with config)...");

    // deploy-host + K8s：启动 rehydrate——list 存量 agent Services 读回
    // nodePort 回填 published 注册表（进程重启后注册表为空，此前的拨号会
    // 全部回退 loopback:容器端口而失败）
    #[cfg(all(feature = "kubernetes", feature = "deploy-host"))]
    if shared_types::is_deploy_host() && RuntimeType::from_env() == RuntimeType::Kubernetes {
        rehydrate_deploy_host_node_ports().await?;
    }

    let docker_manager_config = build_docker_manager_config(config);

    if let Err(e) =
        docker_manager::global::init_global_docker_manager_with_config(docker_manager_config).await
    {
        error!("Docker Manager initialization failed: {}", e);
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host() {
            error!("[deploy-host] 宿主机形态连接 Docker 失败，请检查：");
            error!("  1. Docker/OrbStack 是否已启动");
            error!(
                "  2. DOCKER_SOCKET_PATH 是否指向有效 socket（OrbStack 备选：$HOME/.orbstack/run/docker.sock）"
            );
            error!("  3. 当前用户是否有 socket 访问权限");
        }
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
    // PG 模式跳过启动清理：project/session/container 映射以 PG 为真源（启动全量加载），
    // 用户容器跨 rcoder 重启存活；孤儿容器由 status_checker/cleaner/pod_ensure 兜底对账。
    // （内存模式保留"重启即推倒重来 + 懒重建"的单节点自愈语义，行为零改动）
    if config.storage.backend == crate::config::StorageBackend::Postgres {
        info!(
            "[STORAGE_PG] startup cleanup skipped (postgres mode: state restored from PG, agent containers survive restarts)"
        );
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
                    let enabled_services =
                        shared_types::get_enabled_service_types(&multi_image_config);
                    if result.successfully_removed > 0 {
                        info!(
                            " Startup cleanup completed, removed {} leftover containers (covering {} service types)",
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
                                failure.container_id, failure.container_name, failure.error_message
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

pub async fn get_container_prefixes(config: &AppConfig) -> anyhow::Result<(String, String)> {
    // K8s 部署下命名真源是 kubernetes_config（pod/PVC 前缀优先读它，见
    // KubernetesRuntime::service_container_prefix）——容器名反解必须与创建侧
    // 同源，否则前缀配置漂移时反解系统性失准。k8s 配置缺失的键回退
    // docker 多镜像链（compose 部署的主源，行为不变）。
    let docker_config = config
        .docker_config
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Docker config is required for container prefix"))?;
    let multi_config = docker_config.get_multi_image_config();
    let selector = docker_manager::image_selector::ImageSelector::new(multi_config);

    let rcoder_cfg = selector
        .get_service_config(&shared_types::ServiceType::WebAgentRunner)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get RCoder service config: {e}"))?;
    let computer_cfg = selector
        .get_service_config(&shared_types::ServiceType::ComputerAgentRunner)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get ComputerAgentRunner service config: {e}"))?;

    let k8s_rcoder = config
        .kubernetes_config
        .as_ref()
        .and_then(|k8s| k8s.get_service_config(&shared_types::ServiceType::WebAgentRunner))
        .map(|cfg| cfg.container_prefix().to_string());
    let k8s_computer = config
        .kubernetes_config
        .as_ref()
        .and_then(|k8s| k8s.get_service_config(&shared_types::ServiceType::ComputerAgentRunner))
        .map(|cfg| cfg.container_prefix().to_string());

    Ok((
        k8s_rcoder.unwrap_or_else(|| rcoder_cfg.container_prefix().to_string()),
        k8s_computer.unwrap_or_else(|| computer_cfg.container_prefix().to_string()),
    ))
}

fn show_docker_configuration_help(socket_path: &str) {
    error!(" Docker config help:");
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
    error!(" Docker socket path:");
    error!(" Linux: /var/run/docker.sock");
    error!("  macOS + Docker Desktop: /var/run/docker.sock");
    error!("  Rootless Docker: /run/user/$UID/docker.sock");
    error!("");
    error!(" troubleshooting:");
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

/// deploy-host K8s 启动回填：list 本 namespace 的 rcoder-runtime agent
/// Services（selector: managed-by=rcoder-runtime + component=agent），按
/// `rcoder.io/identifier` label 取 identifier，读回 nodePort 整表登记注册表。
#[cfg(all(feature = "kubernetes", feature = "deploy-host"))]
async fn rehydrate_deploy_host_node_ports() -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Service;
    use kube::api::{Api, ListParams};

    let host =
        shared_types::published::k8s_node_port_host().map_err(|error| anyhow::anyhow!(error))?;

    let client = kube::Client::try_default()
        .await
        .map_err(|e| anyhow::anyhow!("deploy-host rehydrate kube client: {e}"))?;
    let namespace = std::env::var("RCODER_K8S_NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let services: Api<Service> = Api::namespaced(client, &namespace);
    let list = services
        .list(&ListParams::default().labels(
            "app.kubernetes.io/managed-by=rcoder-runtime,app.kubernetes.io/component=agent",
        ))
        .await
        .map_err(|e| anyhow::anyhow!("deploy-host rehydrate list services: {e}"))?;
    let mut registered = 0usize;
    for svc in list.items {
        let Some(identifier) = svc
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("rcoder.io/identifier"))
            .cloned()
        else {
            continue;
        };
        let Some(spec) = svc
            .spec
            .as_ref()
            .filter(|spec| spec.type_.as_deref() == Some("NodePort"))
        else {
            continue;
        };
        let Some(ports) = spec.ports.as_ref() else {
            continue;
        };
        let map: std::collections::HashMap<u16, u16> = ports
            .iter()
            .filter_map(|p| {
                let node_port = p.node_port? as u16;
                Some((p.port as u16, node_port))
            })
            .collect();
        if map.len() != ports.len() || map.is_empty() {
            tracing::warn!(
                identifier,
                "[deploy-host] incomplete NodePort Service; skipping route rehydrate"
            );
            continue;
        }
        // 双键对齐运行期注册（k8s_service.rs：identifier + 完整 STS 名）——
        // 重启后 get_container_info 的查询键是 pod_info.container_name
        // （STS 完整名），单键会让首次拨号回退容器端口
        if let Some(svc_name) = svc.metadata.name.as_deref()
            && let Some(sts_name) = svc_name.strip_suffix("-svc")
        {
            shared_types::published::register_node_ports(sts_name, host, map.clone());
        }
        shared_types::published::register_node_ports(&identifier, host, map);
        registered += 1;
    }
    tracing::info!(
        "[deploy-host] K8s NodePort rehydrate: {registered} agent service(s) registered"
    );
    Ok(())
}

/// deploy-host Docker 重启回填：list 存量 rcoder 容器按 Reach 模式重新登记
/// （K8s 侧 rehydrate 的 Docker 对应——重启后注册表为空则存量 agent 容器
/// 拨号全部回退 loopback:容器端口）。
///
/// 过滤：仅 **running**（stopped 容器 IP 已失效，登记死 IP 比缺项更糟）且带
/// `service-type` label（非 rcoder 管理容器不占注册表）。键集与创建链同源：
/// `.Name` 去斜杠 ∪ `identifier`（starter）∪ `app-id`（app_create）。
#[cfg(feature = "deploy-host")]
async fn rehydrate_deploy_host_docker_ports() {
    use bollard::query_parameters::{InspectContainerOptions, ListContainersOptions};

    let Ok(docker) = bollard::Docker::connect_with_local_defaults() else {
        tracing::warn!("[deploy-host] Docker rehydrate: connect failed");
        return;
    };
    let Ok(containers) = docker.list_containers(None::<ListContainersOptions>).await else {
        tracing::warn!("[deploy-host] Docker rehydrate: list failed");
        return;
    };
    // Direct 模式 preferred 网卡与创建链 get_main_network_name 同源
    // （network_management ensure_deploy_host_network 的 env 解析）
    let preferred = std::env::var("RCODER_DEPLOY_HOST_NETWORK")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rcoder-agent-network".to_owned());
    let mut registered = 0usize;
    let mut skipped_stopped = 0usize;
    let mut skipped_unmanaged = 0usize;
    let mut failed = 0usize;
    for container in &containers {
        let Some(inspect_id) = container.id.as_deref() else {
            continue;
        };
        // 无 service-type label = 非 rcoder 管理容器，跳过且不 inspect
        let labels = container.labels.as_ref();
        if !labels.is_some_and(|l| l.contains_key("service-type")) {
            skipped_unmanaged += 1;
            continue;
        }
        let observation = shared_types::published::begin_physical_observation(inspect_id);
        let Ok(inspect) = docker
            .inspect_container(inspect_id, None::<InspectContainerOptions>)
            .await
        else {
            failed += 1;
            continue;
        };
        let running = inspect
            .state
            .as_ref()
            .and_then(|state| state.status)
            .is_some_and(|status| status == bollard::models::ContainerStateStatusEnum::RUNNING);
        if !running {
            skipped_stopped += 1;
            continue;
        }
        let name_key = inspect
            .name
            .as_deref()
            .map(|name| name.trim_start_matches('/').to_owned())
            .unwrap_or_else(|| inspect_id.to_owned());
        let identifier_key = labels.and_then(|l| l.get("identifier")).cloned();
        let app_id_key = labels
            .and_then(|l| l.get(shared_types::USERAPP_DOCKER_APP_ID_LABEL))
            .cloned();
        for key in [Some(name_key), identifier_key, app_id_key]
            .into_iter()
            .flatten()
        {
            match docker_manager::deploy_host_ports::register_reach_from_inspect(
                &key,
                Some(preferred.as_str()),
                &inspect,
                &observation,
            ) {
                Ok(()) => registered += 1,
                Err(_) => failed += 1,
            }
        }
    }
    tracing::info!(
        "[deploy-host] Docker rehydrate: {registered} registration(s) \
         (skipped: {skipped_stopped} stopped, {skipped_unmanaged} unmanaged, {failed} failed)"
    );
}

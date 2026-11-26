//! 配置里的挂载点（`service_config.mounts`）：解析模板、处理与自动注入的冲突、预建宿主
//! 目录、给日志挂载加环境变量。

use std::collections::{HashMap, HashSet};

use tracing::{debug, info, warn};

use super::log_mount::{create_log_directory, is_log_mount, require_new_log_directory};
use super::templates::{directory_visible_to_this_process, path_after_resolved_base, substitute};
use super::{HostPathLookup, MountContext};
use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;

/// 按配置逐条加挂载。`auto_injected_paths` 是自动注入已经占用的容器内路径
pub(super) async fn add_config_mounts(
    mut builder: ContainerConfigBuilder,
    ctx: &MountContext<'_>,
    lookup: &dyn HostPathLookup,
    auto_injected_paths: &HashSet<String>,
) -> crate::DockerResult<ContainerConfigBuilder> {
    let service_config = ctx.service_config;
    let variables = ctx.variables;
    let workspace_resolution = ctx.workspace_resolution;
    let container_id = ctx.container_id;
    let container_prefix = ctx.container_prefix;

    // 🎯 处理配置文件中的挂载点 (service_config.mounts)
    // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
    let container_name =
        crate::utils::DockerUtils::generate_container_name(container_prefix, container_id)
            .map_err(DockerError::ConfigurationError)?;
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d%H%M%S").to_string();
    let log_dir_name = crate::log_directory::log_directory_name(&container_name, now);

    // 设置命令（放在获取 container_name 之后，因为会移动 service_config.command）
    builder = builder.command(service_config.command.clone());
    if let Some(entry) = service_config.entrypoint.clone() {
        builder = builder.entrypoint(entry);
    }

    // 基础变量集
    let mut base_variables = variables.clone();
    base_variables.insert("container_name".to_string(), container_name.clone());
    base_variables.insert("timestamp".to_string(), timestamp.clone());
    base_variables.insert("log_dir_name".to_string(), log_dir_name.clone());

    // 缓存已解析的路径，避免重复解析
    let mut resolved_paths_cache: HashMap<String, std::path::PathBuf> = HashMap::new();

    // 添加配置文件中定义的挂载点
    for mount_config in &service_config.mounts {
        // 跳过 workspace 挂载：auto-inject 统一处理隔离挂载
        // 精确匹配 resolve_from，不靠路径模式猜测
        if mount_config.resolve_from.as_deref() == Some(workspace_resolution) {
            debug!(
                "Skipping workspace mount from config (auto-inject handles isolation): \
                 container_path={}, resolve_from={}",
                mount_config.container_path,
                mount_config.resolve_from.as_deref().unwrap_or("")
            );
            continue;
        }

        let mut mount_variables = base_variables.clone();

        // 如果配置了 resolve_from，解析动态路径
        if let Some(ref resolve_from_path) = mount_config.resolve_from {
            // 检查缓存（只缓存基础路径解析结果）
            let resolved_base = if let Some(cached) = resolved_paths_cache.get(resolve_from_path) {
                Some(cached.clone())
            } else {
                // 解析 resolve_from 路径到宿主机基础路径
                match lookup.host_path(resolve_from_path).await {
                    Ok(host_base_path) => {
                        info!(
                            "[DOCKER_MGR] Resolved from {} to host path: {}",
                            resolve_from_path,
                            host_base_path.display()
                        );
                        // 缓存基础路径解析结果
                        resolved_paths_cache
                            .insert(resolve_from_path.clone(), host_base_path.clone());
                        Some(host_base_path)
                    }
                    Err(e) => {
                        warn!(
                            "[DOCKER_MGR] Unable to resolve path (resolve_from: {}): {}",
                            resolve_from_path, e
                        );
                        None
                    }
                }
            };

            // 添加解析后的基础路径变量
            if let Some(resolved_path) = resolved_base {
                let normalized = resolved_path.components().collect::<std::path::PathBuf>();
                mount_variables.insert(
                    "resolved_path".to_string(),
                    normalized.to_string_lossy().to_string(),
                );
            } else {
                // 如果解析失败，跳过此挂载点
                warn!(
                    "[DOCKER_MGR] Skipping mount point (unable to resolve resolve_from): {}",
                    mount_config.container_path
                );
                continue;
            }
        }

        // 解析宿主机路径中的变量，并用 PathBuf 规范化（消除多余的斜杠）
        let resolve_host_path = |variables: &HashMap<String, String>| {
            std::path::PathBuf::from(mount_config.resolve_host_path(variables))
                .components()
                .collect::<std::path::PathBuf>()
                .to_string_lossy()
                .to_string()
        };
        let mut resolved_host_path = resolve_host_path(&mount_variables);

        // 检查是否还有未替换的变量（如 {logs_host_path} 等）
        if resolved_host_path.contains('{') && resolved_host_path.contains('}') {
            warn!(
                "[DOCKER_MGR] Skipping mount point (host_path contains unresolved variables): {}",
                resolved_host_path
            );
            continue;
        }

        // 解析容器路径中的变量
        let resolve_container_path =
            |values: &HashMap<String, String>| substitute(&mount_config.container_path, values);
        let mut resolved_container_path = resolve_container_path(&mount_variables);

        info!(
            "Adding mount point: {} -> {} (read_only: {})",
            resolved_host_path, resolved_container_path, mount_config.read_only
        );

        // 冲突检测：如果 config 挂载的 container_path 与 auto-inject 重复，丢弃 config 挂载
        if auto_injected_paths.contains(&resolved_container_path) {
            warn!(
                "[DOCKER_MGR] Skipping config mount (conflicts with auto-inject): \
                 {} -> {}",
                resolved_container_path, resolved_host_path
            );
            continue;
        }

        let log_mount = is_log_mount(mount_config);

        // 日志目录一次性使用：独占新建这个容器的日志目录。同名目录已存在时会换个名字，
        // 绑定的路径和 CONTAINER_LOG_NAME 都跟着用最终的名字
        let mut log_dir_name = log_dir_name.clone();
        if log_mount {
            require_new_log_directory(mount_config)?;
            if !mount_config.read_only
                && let Some(name) = create_log_directory(mount_config, &mount_variables).await?
            {
                log_dir_name = name;
                mount_variables.insert("log_dir_name".to_string(), log_dir_name.clone());
                resolved_host_path = resolve_host_path(&mount_variables);
                resolved_container_path = resolve_container_path(&mount_variables);
            }
        }

        // 确保目录存在（仅对非只读挂载创建目录；日志目录在上面已经新建）
        // 重要：Docker bind mount 要求宿主机路径必须存在
        //
        // 由于代码运行在容器内，无法直接访问宿主机路径（如 /Volumes/soddygo/...）
        // 必须使用容器内通过 volume 挂载可访问的路径来创建目录
        //
        // 策略：必须配置 resolve_from 才能正确创建目录
        // 使用 resolve_from + 相对路径 来创建目录
        if !mount_config.read_only && !log_mount {
            if let Some(ref resolve_from_path) = mount_config.resolve_from {
                // host_path 模板里 {resolved_path} 之后的部分，例如 "/{project_id}"
                let relative_part = path_after_resolved_base(mount_config, &mount_variables);
                let create_path = directory_visible_to_this_process(
                    &resolved_host_path,
                    resolve_from_path,
                    &relative_part,
                );
                debug!(
                    "Creating directory using container path: {} (resolve_from: {}, relative: {})",
                    create_path, resolve_from_path, relative_part
                );

                if let Err(e) = tokio::fs::create_dir_all(&create_path).await {
                    warn!(
                        "[DOCKER_MGR] createdmountdirectoryfailed: {} - {}",
                        create_path, e
                    );
                } else {
                    info!("[DOCKER_MGR] directorycreatedsucceeded: {}", create_path);
                }
            } else {
                // 没有配置 resolve_from，无法在容器内创建宿主机路径
                // 假设目录已存在，跳过创建
                info!(
                    "Skipping directory creation (resolve_from not configured): {}",
                    resolved_host_path
                );
            }
        }

        // 如果是日志挂载，添加环境变量。CONTAINER_LOGS_DIR 写实际挂载点（变量已替换）：builder
        // 重启靠它认出归档里的日志挂载
        if log_mount {
            builder = builder.env("CONTAINER_LOGS_DIR", &resolved_container_path);
            builder = builder.env("CONTAINER_LOG_NAME", &log_dir_name);
        }

        builder = builder.add_mount(crate::MountPoint {
            host_path: resolved_host_path,
            container_path: resolved_container_path,
            read_only: mount_config.read_only,
        });
    }

    Ok(builder)
}

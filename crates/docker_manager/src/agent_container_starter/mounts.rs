//! agent 容器挂载构建（从 agent_container_starter.rs 拆出）。
//!
//! `apply_auto_mounts`：workspace 隔离挂载 auto-inject（service_type/隔离级别
//! 决定挂载策略）+ config mounts 解析（冲突检测/宿主目录预创建/日志挂载 env）。

use std::collections::HashMap;
use std::str::FromStr;

use tracing::{debug, info, warn};

use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;
use chrono::Utc;
use shared_types::{IsolationType, ServiceImageConfig, ServiceType};

/// 挂载构建所需上下文（start() 的局部变量打包）。
pub(super) struct MountContext<'a> {
    pub(super) container_id: &'a str,
    pub(super) container_prefix: &'a str,
    pub(super) service_config: &'a ServiceImageConfig,
    pub(super) variables: &'a HashMap<String, String>,
    pub(super) workspace_resolution: &'a str,
    pub(super) workspace_container: &'a str,
    pub(super) service_type: &'a ServiceType,
    pub(super) isolation_type: Option<&'a str>,
    pub(super) project_id: Option<&'a str>,
    pub(super) user_id: Option<&'a str>,
    pub(super) pod_id: Option<&'a str>,
    pub(super) tenant_id: Option<&'a str>,
    pub(super) space_id: Option<&'a str>,
}

pub(super) async fn apply_auto_mounts(
    mut builder: ContainerConfigBuilder,
    ctx: &MountContext<'_>,
) -> crate::DockerResult<ContainerConfigBuilder> {
    let container_id = ctx.container_id;
    let container_prefix = ctx.container_prefix;
    let service_config = ctx.service_config;
    let variables = ctx.variables;
    let workspace_resolution = ctx.workspace_resolution;
    let workspace_container = ctx.workspace_container;
    let service_type = ctx.service_type;
    let isolation_type = ctx.isolation_type;
    let project_id = ctx.project_id;
    let user_id = ctx.user_id;
    let pod_id = ctx.pod_id;
    let tenant_id = ctx.tenant_id;
    let space_id = ctx.space_id;

    // ===== 自动注入 workspace 挂载 =====
    // workspace_resolution: rcoder 容器内路径 → 解析宿主机路径
    // workspace_container: sub-container 内挂载目标
    //
    // auto-inject 统一处理 workspace 隔离挂载，config mounts 中的 workspace 挂载
    // 通过 resolve_from 精确匹配跳过，避免重复。

    // 记录 auto-inject 已添加的 container_path，用于后续冲突检测
    let mut auto_injected_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    match crate::path::resolve_container_path_to_host(std::path::Path::new(&workspace_resolution))
        .await
    {
        Ok(workspace_host_path) => {
            let (host_sub, container_mount) = if pod_id.is_some() {
                // pod_id 有值：根据 isolation_type 决定挂载级别
                // 必须校验必填字段，遵循 Fail Fast 原则
                let isolation = match isolation_type {
                    Some(s) => IsolationType::from_str(s)
                        .map_err(|e| DockerError::ContainerCreationError(e.to_string()))?,
                    None => IsolationType::default(),
                };

                match isolation {
                    IsolationType::Space => {
                        // space 隔离需要 tenant_id 和 space_id
                        let tid = tenant_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "tenant_id is required when isolation_type is 'space' and pod_id is provided".to_string()
                            )
                        })?;
                        let sid = space_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "space_id is required when isolation_type is 'space' and pod_id is provided".to_string()
                            )
                        })?;
                        let sub = format!("{}/{}", tid, sid);
                        (
                            sub.clone(),
                            std::path::PathBuf::from(&workspace_container).join(&sub),
                        )
                    }
                    IsolationType::Tenant => {
                        // tenant 隔离只需要 tenant_id
                        let tid = tenant_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "tenant_id is required when isolation_type is 'tenant' and pod_id is provided".to_string()
                            )
                        })?;
                        let sub = tid.to_string();
                        (
                            sub.clone(),
                            std::path::PathBuf::from(&workspace_container).join(&sub),
                        )
                    }
                    IsolationType::Project => {
                        // project 隔离需要 tenant_id、space_id 和 project_id
                        let tid = tenant_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "tenant_id is required when pod_id is provided".to_string(),
                            )
                        })?;
                        let sid = space_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "space_id is required when pod_id is provided".to_string(),
                            )
                        })?;
                        let proj = project_id.ok_or_else(|| {
                            DockerError::ContainerCreationError(
                                "project_id is required when pod_id is provided".to_string(),
                            )
                        })?;
                        let sub = format!("{}/{}/{}", tid, sid, proj);
                        (
                            sub.clone(),
                            std::path::PathBuf::from(&workspace_container).join(&sub),
                        )
                    }
                }
            } else {
                // pod_id 无值：根据 service_type 选择挂载策略
                match service_type {
                    // ComputerAgentRunner: 一个 user_id 对应一个容器
                    // 挂载: 宿主机 /computer-project-workspace/{user_id} → 容器 /home/user
                    // config.yml 中 container_path: "/home/user"
                    ServiceType::ComputerAgentRunner => {
                        let uid = user_id.unwrap_or("default");
                        (
                            uid.to_string(),
                            std::path::PathBuf::from(&workspace_container),
                        )
                    }
                    // UserappBuilder 完整开发容器（per-app）——挂载压平:
                    // 宿主 {根}/dev/{user_id}/{app_id} → 容器 /home/user/{app_id}
                    // （env/user 层只在宿主树, 容器内不体现）; data/logs/agent-store 三个
                    // 兄弟挂载在主挂载之后追加。user_id 缺失兜底 app_id（防御旧调用方,
                    // create_builder_and_register 已传真实值）。
                    // resolution path 由 config user-app-builder 段配置为
                    // /app/userapp-workspace, 经 rcoder compose bind 反解宿主根。
                    ServiceType::UserappBuilder => {
                        let pid = project_id.unwrap_or("default");
                        let uid = user_id.unwrap_or(pid);
                        // 宿主子路径 = 挂载压平四目录之一（布局单一事实源 paths::userapp_dev_subpaths）
                        (
                            shared_types::paths::userapp_dev_subpaths(uid, pid)[0].clone(),
                            std::path::PathBuf::from(shared_types::paths::USERAPP_DEV_HOME)
                                .join(pid),
                        )
                    }
                    // RCoder/Userapp: 一个 project_id 对应一个容器
                    // 挂载: 宿主机 /project_workspace/{project_id} → 容器 /project_workspace/{project_id}
                    ServiceType::WebAgentRunner | ServiceType::Userapp => {
                        let pid = project_id.unwrap_or("default");
                        (
                            pid.to_string(),
                            std::path::PathBuf::from(&workspace_container).join(pid),
                        )
                    }
                }
            };

            let host_mount = workspace_host_path.join(&host_sub);

            // 创建宿主机挂载目录（通过容器内路径创建，volume 会传播到宿主机）
            // workspace_resolution 是容器内路径（如 /app/computer-project-workspace）
            // host_sub 是子目录（如 tenant_abc）
            // 拼接后在容器内创建目录，通过 docker-compose volume 自动同步到宿主机
            let host_dir_to_create =
                std::path::PathBuf::from(&workspace_resolution).join(&host_sub);
            if let Err(e) = std::fs::create_dir_all(&host_dir_to_create) {
                warn!(
                    "[DOCKER_MGR] Failed to create workspace directory {}: {}",
                    host_dir_to_create.display(),
                    e
                );
            } else {
                info!(
                    "[DOCKER_MGR] Created workspace directory: {}",
                    host_dir_to_create.display()
                );
            }

            {
                let host_mount_str = host_mount.to_string_lossy().to_string();
                let container_mount_str = container_mount.to_string_lossy().to_string();
                auto_injected_paths.insert(container_mount_str.clone());
                builder = builder.add_mount(crate::MountPoint {
                    host_path: host_mount_str,
                    container_path: container_mount_str,
                    read_only: false,
                });

                info!(
                    "[DOCKER_MGR] Auto workspace mount: {} -> {}",
                    host_mount.display(),
                    container_mount.display()
                );
            }

            // UserappBuilder 追加三个数据挂载（压平模型四挂载的后三个）:
            // 宿主 {根}/dev/{user_id}/data/{app_id} → 容器 /home/user/data（PG/dbx
            // 持久数据, env 注入见 start()）、{根}/dev/{user_id}/logs/{app_id} →
            // /home/user/logs（容器级持久日志, USERAPP_LOG_DIR 约定）、
            // {根}/dev/{user_id}/agent-store/{app_id} → /home/user/.agent-store
            // （file-server agent_store.rs 的 user_root/.agent-store 契约; workspace
            // 内相对软链 ../.agent-store/... 恰好解析到该挂载点）。
            // 与 K8s 四 subPath 挂载（同一块 PVC 卷内）容器内路径完全同构。
            // 注: 宿主树 symlink 从宿主视角断链（多 app 分层 vs 容器内拍平的固有
            // 差异）——宿主侧只删不解析（purge），无影响。
            if pod_id.is_none() && matches!(service_type, ServiceType::UserappBuilder) {
                let pid = project_id.unwrap_or("default");
                let uid = user_id.unwrap_or(pid);
                // data/logs/agent-store = 布局四目录的后三段（单一事实源），与
                // 容器内挂载点按序配对
                let subs = shared_types::paths::userapp_dev_subpaths(uid, pid);
                for (sub, container_path) in [
                    (subs[1].clone(), shared_types::paths::USERAPP_DEV_DATA),
                    (subs[2].clone(), shared_types::paths::USERAPP_DEV_LOGS),
                    (
                        subs[3].clone(),
                        shared_types::paths::USERAPP_DEV_AGENT_STORE,
                    ),
                ] {
                    let hm = workspace_host_path.join(&sub);
                    // 宿主目录预创建（rcoder 容器内经 bind 同步宿主; bind 源必须存在）
                    let hdc = std::path::PathBuf::from(workspace_resolution).join(&sub);
                    if let Err(e) = std::fs::create_dir_all(&hdc) {
                        warn!(
                            "[DOCKER_MGR] Failed to create dev data directory {}: {}",
                            hdc.display(),
                            e
                        );
                    }
                    let hm_str = hm.to_string_lossy().to_string();
                    let cm_str = container_path.to_string();
                    auto_injected_paths.insert(cm_str.clone());
                    builder = builder.add_mount(crate::MountPoint {
                        host_path: hm_str,
                        container_path: cm_str,
                        read_only: false,
                    });
                    info!(
                        "[DOCKER_MGR] Auto dev data mount: {} -> {}",
                        hm.display(),
                        container_path
                    );
                }
            }
        }
        Err(e) => {
            if matches!(service_type, ServiceType::UserappBuilder) {
                // builder 的 workspace 是"数据即产品"卷（开发源码+制品）：
                // 解析失败若继续，容器健康照常但所有开发数据落 overlay 临时层、
                // 回收即丢——fail fast 优于静默降级（computer/web 容器可降级）
                return Err(DockerError::ContainerCreationError(format!(
                    "UserappBuilder workspace host path resolve failed                          (rcoder 容器需挂载 userapp-workspace 锚点): {e}"
                )));
            }
            warn!(
                "[DOCKER_MGR] Failed to resolve workspace host path, skipping auto mount: {}",
                e
            );
        }
    }

    // 🎯 处理配置文件中的挂载点 (service_config.mounts)
    // 容器名统一走 DockerUtils::generate_container_name（含合法性校验，与创建路径一致）
    let container_name =
        crate::utils::DockerUtils::generate_container_name(container_prefix, container_id)
            .map_err(DockerError::ConfigurationError)?;
    let timestamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
    let log_dir_name = format!("{}-{}", container_name, timestamp);

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
                match crate::path::resolve_container_path_to_host(std::path::Path::new(
                    resolve_from_path,
                ))
                .await
                {
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

        // 解析宿主机路径中的变量
        let resolved_host_path = mount_config.resolve_host_path(&mount_variables);

        // 检查是否还有未替换的变量（如 {logs_host_path} 等）
        if resolved_host_path.contains('{') && resolved_host_path.contains('}') {
            warn!(
                "[DOCKER_MGR] Skipping mount point (host_path contains unresolved variables): {}",
                resolved_host_path
            );
            continue;
        }

        // 使用 PathBuf 规范化路径（消除多余的斜杠）
        let resolved_host_path = std::path::PathBuf::from(&resolved_host_path)
            .components()
            .collect::<std::path::PathBuf>()
            .to_string_lossy()
            .to_string();

        // 解析容器路径中的变量
        let mut resolved_container_path = mount_config.container_path.clone();
        for (key, value) in &mount_variables {
            resolved_container_path =
                resolved_container_path.replace(&format!("{{{}}}", key), value);
        }

        info!(
            "Adding mount point: {} -> {} (read_only: {})",
            resolved_host_path, resolved_container_path, mount_config.read_only
        );

        // 确保目录存在（仅对非只读挂载创建目录）
        // 重要：Docker bind mount 要求宿主机路径必须存在
        //
        // 由于代码运行在容器内，无法直接访问宿主机路径（如 /Volumes/soddygo/...）
        // 必须使用容器内通过 volume 挂载可访问的路径来创建目录
        //
        // 策略：必须配置 resolve_from 才能正确创建目录
        // 使用 resolve_from + 相对路径 来创建目录
        if !mount_config.read_only {
            if let Some(ref resolve_from_path) = mount_config.resolve_from {
                // 非 pod 模式：从 host_path 模板中提取相对路径部分
                // host_path 格式通常是 "{resolved_path}/{variable}"
                // 我们需要取 {resolved_path} 之后的部分，并拼接到 resolve_from 上
                let host_path_template = &mount_config.host_path;
                let relative_part = if host_path_template.starts_with("{resolved_path}") {
                    // 提取 {resolved_path} 之后的模板部分并替换变量
                    let suffix = host_path_template
                        .strip_prefix("{resolved_path}")
                        .unwrap_or("");
                    let mut resolved_suffix = suffix.to_string();
                    for (key, value) in &mount_variables {
                        resolved_suffix = resolved_suffix.replace(&format!("{{{}}}", key), value);
                    }
                    resolved_suffix
                } else {
                    // 如果不是以 {resolved_path} 开头，尝试计算相对路径
                    // 这种情况不太常见，使用空字符串作为后备
                    String::new()
                };

                // 构建容器内可访问的路径
                let create_path = format!("{}{}", resolve_from_path, relative_part);
                debug!(
                    "Creating directory using container path: {} (resolve_from: {}, relative: {})",
                    create_path, resolve_from_path, relative_part
                );

                if let Err(e) = std::fs::create_dir_all(&create_path) {
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

        // 冲突检测：如果 config 挂载的 container_path 与 auto-inject 重复，丢弃 config 挂载
        if auto_injected_paths.contains(&resolved_container_path) {
            warn!(
                "[DOCKER_MGR] Skipping config mount (conflicts with auto-inject): \
                 {} -> {}",
                resolved_container_path, resolved_host_path
            );
            continue;
        }

        builder = builder.add_mount(crate::MountPoint {
            host_path: resolved_host_path,
            container_path: resolved_container_path,
            read_only: mount_config.read_only,
        });

        // 如果是日志挂载，添加环境变量
        if mount_config.container_path.contains("container-logs") {
            builder = builder.env("CONTAINER_LOGS_DIR", &mount_config.container_path);
            builder = builder.env("CONTAINER_LOG_NAME", &log_dir_name);
        }
    }

    Ok(builder)
}

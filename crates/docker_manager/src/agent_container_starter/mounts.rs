//! agent 容器挂载构建（从 agent_container_starter.rs 拆出）。
//!
//! `apply_auto_mounts`：workspace 隔离挂载 auto-inject（service_type/隔离级别
//! 决定挂载策略）+ config mounts 解析（冲突检测/宿主目录预创建/日志挂载 env）。
//!
//! 日志挂载（容器内路径带 `container-logs`）的目录是一次性的：每个容器绑定的都是启动它
//! 时独占新建的目录，已存在的同名目录不复用（见 `crate::log_directory`）。日志清理器据此
//! 不必和这里的挂载准备互斥。

use std::collections::HashMap;

use std::str::FromStr;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;
use shared_types::{IsolationType, ServiceImageConfig, ServiceMountConfig, ServiceType};

/// 日志目录名在 `host_path` 模板里的变量
const LOG_DIR_NAME_VARIABLE: &str = "{log_dir_name}";

/// 把 RCoder 进程里的路径换算成宿主机路径
#[async_trait]
pub(super) trait HostPathLookup: Send + Sync {
    async fn host_path(&self, container_path: &str) -> crate::DockerResult<std::path::PathBuf>;
}

/// 按 RCoder 所在容器的挂载表换算；deploy-host 形态下进程路径就是宿主机路径
pub(super) struct ProcessMounts;

#[async_trait]
impl HostPathLookup for ProcessMounts {
    async fn host_path(&self, container_path: &str) -> crate::DockerResult<std::path::PathBuf> {
        crate::path::resolve_container_path_to_host(std::path::Path::new(container_path)).await
    }
}

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
    lookup: &dyn HostPathLookup,
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

    match lookup.host_path(workspace_resolution).await {
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
                    // Computer 族（含常规项目，共享容器）: 一个 user_id 对应一个容器
                    // 挂载: 宿主机 /computer-project-workspace/{user_id} → 容器 /home/user
                    // config.yml 中 container_path: "/home/user"
                    ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                        let uid = user_id.unwrap_or("default");
                        (
                            uid.to_string(),
                            std::path::PathBuf::from(&workspace_container),
                        )
                    }
                    // UserappBuilder 完整开发容器（per-app）——挂载压平:
                    // 宿主 {根}/dev/userapp/{app_id} → 容器 /home/user/{app_id}
                    // （env/user 层只在宿主树, 容器内不体现）; data/logs/agent-store 三个
                    // 兄弟挂载在主挂载之后追加。应用共享：固定命名空间，无用户段。
                    // resolution path 由 config user-app-builder 段配置为
                    // /app/userapp-workspace, 经 rcoder compose bind 反解宿主根。
                    ServiceType::UserappBuilder => {
                        let pid = project_id.unwrap_or("default");
                        // 宿主子路径 = 挂载压平四目录之一（布局单一事实源 paths::userapp_dev_subpaths）
                        (
                            shared_types::paths::userapp_dev_subpaths(pid)[0].clone(),
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

            // 创建宿主机挂载目录。容器形态：经容器内路径创建（compose bind 双向
            // 同步宿主机）。deploy-host 宿主机形态：rcoder 进程即宿主机进程，
            // 直接在 bind 源（host_mount）上创建。
            #[cfg(feature = "deploy-host")]
            let host_dir_to_create = if shared_types::is_deploy_host() {
                host_mount.clone()
            } else {
                std::path::PathBuf::from(&workspace_resolution).join(&host_sub)
            };
            #[cfg(not(feature = "deploy-host"))]
            let host_dir_to_create =
                std::path::PathBuf::from(&workspace_resolution).join(&host_sub);
            if let Err(e) = tokio::fs::create_dir_all(&host_dir_to_create).await {
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
                // data/logs/agent-store = 布局四目录的后三段（单一事实源），与
                // 容器内挂载点按序配对
                let subs = shared_types::paths::userapp_dev_subpaths(pid);
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
                    #[cfg(feature = "deploy-host")]
                    let hdc = if shared_types::is_deploy_host() {
                        hm.clone()
                    } else {
                        std::path::PathBuf::from(&workspace_resolution).join(&sub)
                    };
                    #[cfg(not(feature = "deploy-host"))]
                    let hdc = std::path::PathBuf::from(&workspace_resolution).join(&sub);
                    if let Err(e) = tokio::fs::create_dir_all(&hdc).await {
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

        builder = builder.add_mount(crate::MountPoint {
            host_path: resolved_host_path,
            container_path: resolved_container_path,
            read_only: mount_config.read_only,
        });

        // 如果是日志挂载，添加环境变量
        if log_mount {
            builder = builder.env("CONTAINER_LOGS_DIR", &mount_config.container_path);
            builder = builder.env("CONTAINER_LOG_NAME", &log_dir_name);
        }
    }

    Ok(builder)
}

/// 把模板里的 `{变量}` 换成 `values` 里的值
fn substitute(template: &str, values: &HashMap<String, String>) -> String {
    values
        .iter()
        .fold(template.to_owned(), |rendered, (key, value)| {
            rendered.replace(&format!("{{{key}}}"), value)
        })
}

/// 这条挂载是不是容器的日志目录：容器内的路径带 `container-logs`
fn is_log_mount(mount_config: &ServiceMountConfig) -> bool {
    mount_config.container_path.contains("container-logs")
}

/// 日志挂载必须绑定 `{log_dir_name}` 目录。绑别的目录等于让新容器复用一个已经存在的目录，
/// 它可能已经过期、正被日志清理器回收
fn require_new_log_directory(mount_config: &ServiceMountConfig) -> crate::DockerResult<()> {
    if mount_config.host_path.ends_with(LOG_DIR_NAME_VARIABLE) {
        return Ok(());
    }
    Err(DockerError::ConfigurationError(format!(
        "Log mount {} must bind a new directory whose name ends with {}; its host path {:?} does not",
        mount_config.container_path, LOG_DIR_NAME_VARIABLE, mount_config.host_path
    )))
}

/// 日志目录一次性使用：独占新建这个容器的日志目录，返回最终的名字。
///
/// 返回 `None` 表示不由 RCoder 建：没配 `resolve_from`，或者建目录时出了错。两种情况都沿用
/// 首选的名字，目录由 Docker 在创建容器时补建（建不出来会告警）。名字全被占不在此列：
/// 沿用哪个都是复用旧目录，直接拒绝。
async fn create_log_directory(
    mount_config: &ServiceMountConfig,
    variables: &HashMap<String, String>,
) -> crate::DockerResult<Option<String>> {
    let (Some(resolve_from), Some(resolved_base)) = (
        mount_config.resolve_from.as_deref(),
        variables.get("resolved_path"),
    ) else {
        return Ok(None);
    };
    let suffix = path_after_resolved_base(mount_config, variables);
    let create_path = directory_visible_to_this_process(resolved_base, resolve_from, &suffix);
    let (Some(parent), Some(preferred)) = (
        std::path::Path::new(&create_path).parent(),
        std::path::Path::new(&create_path)
            .file_name()
            .and_then(|name| name.to_str()),
    ) else {
        return Err(DockerError::ConfigurationError(format!(
            "Log mount {} has no directory to create: {:?}",
            mount_config.container_path, create_path
        )));
    };
    match crate::log_directory::create_fresh_directory(parent, preferred).await {
        Ok(created) => Ok(created
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)),
        Err(error @ crate::log_directory::FreshDirectoryError::Exhausted) => {
            Err(DockerError::ContainerCreationError(format!(
                "Cannot create a new log directory for mount {}: {error} ({})",
                mount_config.container_path,
                parent.display()
            )))
        }
        Err(crate::log_directory::FreshDirectoryError::Io(error)) => {
            warn!(
                "[DOCKER_MGR] createdmountdirectoryfailed: {} - {}",
                create_path, error
            );
            Ok(None)
        }
    }
}

/// `host_path` 模板里 `{resolved_path}` 之后的部分，变量已替换；模板不是以 `{resolved_path}`
/// 开头时为空
fn path_after_resolved_base(
    mount_config: &ServiceMountConfig,
    values: &HashMap<String, String>,
) -> String {
    substitute(
        mount_config
            .host_path
            .strip_prefix("{resolved_path}")
            .unwrap_or(""),
        values,
    )
}

/// RCoder 进程里用来建目录的路径，它对应宿主机上的 `host_path`（`{resolved_path}` 之后的
/// 部分是 `suffix`）。
///
/// 容器形态：用容器内的路径（compose bind 双向同步宿主机，rcoder 进程经挂载卷可写）。
/// deploy-host：rcoder 进程即宿主机进程，直接在 bind 源上创建——容器常量路径在宿主机常为
/// 只读根。
fn directory_visible_to_this_process(host_path: &str, resolve_from: &str, suffix: &str) -> String {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return host_path.to_owned();
    }
    #[cfg(not(feature = "deploy-host"))]
    let _ = host_path;
    format!("{resolve_from}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const CONTAINER: &str = "computer-agent-runner-user_42";

    /// 一次测试用的目录布局：`host` 是 daemon 看到的日志总根，`visible` 是同一个目录在 RCoder
    /// 进程里的样子（一个符号链接）
    struct Layout {
        root: tempfile::TempDir,
    }

    impl Layout {
        fn new() -> Self {
            let layout = Self {
                root: tempfile::tempdir().unwrap(),
            };
            std::fs::create_dir_all(layout.host_log_root()).unwrap();
            std::fs::create_dir(layout.root.path().join("visible")).unwrap();
            std::os::unix::fs::symlink("../host/container", layout.visible_log_root()).unwrap();
            layout
        }

        fn host_log_root(&self) -> PathBuf {
            self.root.path().join("host/container")
        }

        fn visible_log_root(&self) -> PathBuf {
            self.root.path().join("visible/container")
        }
    }

    /// 假的路径换算：只有日志总根解析得出来，别的路径一律失败（工作区挂载因此被跳过）
    struct FakeLookup {
        visible: PathBuf,
        host: PathBuf,
    }

    #[async_trait]
    impl HostPathLookup for FakeLookup {
        async fn host_path(&self, container_path: &str) -> crate::DockerResult<PathBuf> {
            if container_path == self.visible.to_str().unwrap() {
                Ok(self.host.clone())
            } else {
                Err(DockerError::ConfigurationError(format!(
                    "no host path for {container_path}"
                )))
            }
        }
    }

    fn mount(
        container_path: &str,
        host_path: &str,
        read_only: bool,
        resolve_from: &Path,
    ) -> ServiceMountConfig {
        ServiceMountConfig {
            container_path: container_path.into(),
            host_path: host_path.into(),
            read_only,
            mount_type: "bind".into(),
            resolve_from: Some(resolve_from.to_str().unwrap().into()),
        }
    }

    fn log_mount(layout: &Layout) -> ServiceMountConfig {
        mount(
            "/app/container-logs",
            "{resolved_path}/{log_dir_name}",
            false,
            &layout.visible_log_root(),
        )
    }

    async fn apply(
        layout: &Layout,
        mounts: Vec<ServiceMountConfig>,
    ) -> crate::DockerResult<crate::DockerContainerConfig> {
        let service_config: ServiceImageConfig = serde_json::from_value(serde_json::json!({
            "service_type": "computer-agent-runner",
            "enabled": true,
            "mounts": mounts,
        }))
        .unwrap();
        let variables = HashMap::new();
        let ctx = MountContext {
            container_id: "user_42",
            container_prefix: "computer-agent-runner",
            service_config: &service_config,
            variables: &variables,
            workspace_resolution: "/app/computer-project-workspace",
            workspace_container: "/home/user",
            service_type: &ServiceType::ComputerAgentRunner,
            isolation_type: None,
            project_id: None,
            user_id: Some("user_42"),
            pod_id: None,
            tenant_id: None,
            space_id: None,
        };
        apply_auto_mounts(
            ContainerConfigBuilder::new("user_42"),
            &ctx,
            &FakeLookup {
                visible: layout.visible_log_root(),
                host: layout.host_log_root(),
            },
        )
        .await?
        .build()
    }

    fn only_mount(config: &crate::DockerContainerConfig) -> &crate::MountPoint {
        let [mount] = config.extra_mounts.as_slice() else {
            panic!("expected exactly one mount: {:?}", config.extra_mounts)
        };
        mount
    }

    fn file_name(path: &str) -> &str {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
    }

    fn entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// 占住接下来几秒里可能用到的名字：首选名，以及 `fallbacks` 个备选名
    fn take_upcoming_names(layout: &Layout, fallbacks: u32) {
        let now = chrono::Utc::now();
        for seconds in [-1, 0, 1, 2] {
            let stamp = (now + chrono::Duration::seconds(seconds)).format("%Y%m%d%H%M%S");
            std::fs::create_dir_all(layout.host_log_root().join(format!("{CONTAINER}-{stamp}")))
                .unwrap();
            for attempt in 2..=fallbacks {
                std::fs::create_dir_all(
                    layout
                        .host_log_root()
                        .join(format!("{CONTAINER}-{stamp}-{attempt}")),
                )
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_new_container_gets_a_log_directory_of_its_own() {
        let layout = Layout::new();

        let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

        let mount = only_mount(&config);
        assert_eq!(mount.container_path, "/app/container-logs");
        assert!(!mount.read_only);
        let name = file_name(&mount.host_path);
        assert_eq!(
            name.strip_prefix(CONTAINER).map(str::len),
            Some(15),
            "the directory is named after the container and the UTC second: {name}"
        );
        assert_eq!(
            mount.host_path,
            layout.host_log_root().join(name).to_str().unwrap()
        );
        assert!(layout.host_log_root().join(name).is_dir());
        assert_eq!(
            config
                .env_vars
                .get("CONTAINER_LOGS_DIR")
                .map(String::as_str),
            Some("/app/container-logs")
        );
        assert_eq!(
            config
                .env_vars
                .get("CONTAINER_LOG_NAME")
                .map(String::as_str),
            Some(name)
        );
        assert_eq!(entries(&layout.host_log_root()), [name.to_owned()]);
    }

    #[tokio::test]
    async fn an_existing_log_directory_is_never_bound_again() {
        let layout = Layout::new();
        take_upcoming_names(&layout, 2);

        let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

        let mount = only_mount(&config);
        let name = file_name(&mount.host_path);
        assert!(
            name.ends_with("-3"),
            "the preferred name and the first fallback are taken: {name}"
        );
        assert!(layout.host_log_root().join(name).is_dir());
        assert_eq!(
            config
                .env_vars
                .get("CONTAINER_LOG_NAME")
                .map(String::as_str),
            Some(name),
            "the container is told the name that was actually bound"
        );
    }

    #[tokio::test]
    async fn a_log_directory_that_cannot_get_a_fresh_name_is_refused() {
        let layout = Layout::new();
        take_upcoming_names(&layout, crate::log_directory::NAME_ATTEMPTS);

        let error = apply(&layout, vec![log_mount(&layout)]).await.unwrap_err();

        assert!(
            matches!(&error, DockerError::ContainerCreationError(message) if message.contains("Cannot create a new log directory")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_log_mount_that_reuses_a_directory_is_rejected() {
        let layout = Layout::new();
        let shared = mount(
            "/app/container-logs",
            "{resolved_path}/shared",
            false,
            &layout.visible_log_root(),
        );

        let error = apply(&layout, vec![shared]).await.unwrap_err();

        assert!(
            matches!(&error, DockerError::ConfigurationError(message) if message.contains("must bind a new directory whose name ends with {log_dir_name}")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_read_only_log_mount_is_not_created_here() {
        let layout = Layout::new();
        let read_only = ServiceMountConfig {
            read_only: true,
            ..log_mount(&layout)
        };

        let config = apply(&layout, vec![read_only]).await.unwrap();

        let mount = only_mount(&config);
        assert!(mount.read_only);
        let name = file_name(&mount.host_path);
        assert_eq!(name.strip_prefix(CONTAINER).map(str::len), Some(15));
        assert!(
            !layout.host_log_root().join(name).exists(),
            "a read-only mount is left for Docker to create"
        );
        assert_eq!(
            config
                .env_vars
                .get("CONTAINER_LOG_NAME")
                .map(String::as_str),
            Some(name)
        );
    }

    /// 别的挂载不受影响：目录已经存在就沿用
    #[tokio::test]
    async fn other_mounts_keep_using_the_existing_directory() {
        let layout = Layout::new();
        let wallpaper = layout.host_log_root().join("wallpaper");
        std::fs::create_dir_all(&wallpaper).unwrap();
        std::fs::write(wallpaper.join("keep.txt"), b"kept").unwrap();

        let config = apply(
            &layout,
            vec![mount(
                "/app/wallpaper",
                "{resolved_path}/wallpaper",
                false,
                &layout.visible_log_root(),
            )],
        )
        .await
        .unwrap();

        let mount = only_mount(&config);
        assert_eq!(mount.container_path, "/app/wallpaper");
        assert_eq!(
            mount.host_path,
            wallpaper.to_str().unwrap(),
            "the bind is the existing directory itself"
        );
        assert_eq!(std::fs::read(wallpaper.join("keep.txt")).unwrap(), b"kept");
        assert_eq!(entries(&wallpaper), ["keep.txt".to_owned()]);
        assert!(!config.env_vars.contains_key("CONTAINER_LOG_NAME"));
    }
}

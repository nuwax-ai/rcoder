//! workspace 隔离挂载的自动注入。
//!
//! `workspace_resolution` 是 RCoder 容器内的路径，换算成宿主机路径后按下表挂进容器；
//! config mounts 里 `resolve_from` 等于它的那条会被跳过，免得重复。
//!
//! | 条件 | 宿主机子路径 | 容器内挂载点 |
//! |------|-------------|-------------|
//! | 有 pod_id，space 隔离 | `{tenant_id}/{space_id}` | `{workspace_container}/{tenant_id}/{space_id}` |
//! | 有 pod_id，tenant 隔离 | `{tenant_id}` | `{workspace_container}/{tenant_id}` |
//! | 有 pod_id，project 隔离 | `{tenant_id}/{space_id}/{project_id}` | 同左，接在 `workspace_container` 后 |
//! | 无 pod_id，Computer 族 | `{user_id}` | `{workspace_container}` |
//! | 无 pod_id，UserappBuilder | 布局四目录的第一个 | `/home/user/{app_id}`，另加 data/logs/agent-store 三个兄弟挂载 |
//! | 无 pod_id，Web/Userapp | `{project_id}` | `{workspace_container}/{project_id}` |

use std::collections::HashSet;
use std::str::FromStr;

use tracing::{info, warn};

use super::{HostPathLookup, MountContext};
use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;
use crate::runtime::docker_runtime::host_path_space::{LogDomainAdmission, LogDomainVerdict};
use shared_types::{IsolationType, ServiceType};

/// 自动注入 workspace 挂载，注入过的容器内路径记进 `auto_injected_paths`。
///
/// 宿主机路径解析失败时：UserappBuilder 直接报错（它的 workspace 是"数据即产品"卷，落到
/// overlay 临时层等于回收即丢）；其余只告警、继续。
///
/// workspace 和数据目录不能落在可回收的日志根下面：清理器会把它们当过期日志回收。判不出在
/// 不在的照常挂载，bind 源登记进 `pending_binds` 到创建结束。
pub(super) async fn inject_workspace_mounts(
    mut builder: ContainerConfigBuilder,
    ctx: &MountContext<'_>,
    lookup: &dyn HostPathLookup,
    admission: &LogDomainAdmission<'_>,
    auto_injected_paths: &mut HashSet<String>,
    pending_binds: &mut crate::log_directory::PendingBinds,
) -> crate::DockerResult<ContainerConfigBuilder> {
    let workspace_resolution = ctx.workspace_resolution;
    let workspace_container = ctx.workspace_container;
    let service_type = ctx.service_type;
    let isolation_type = ctx.isolation_type;
    let project_id = ctx.project_id;
    let user_id = ctx.user_id;
    let pod_id = ctx.pod_id;
    let tenant_id = ctx.tenant_id;
    let space_id = ctx.space_id;

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
            if check_data_mount(admission, &container_mount, &host_mount).await? {
                pending_binds
                    .claim(&host_mount, admission.space())
                    .await
                    .map_err(|error| DockerError::ContainerCreationError(error.to_string()))?;
            }

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
                    if check_data_mount(admission, std::path::Path::new(container_path), &hm)
                        .await?
                    {
                        pending_binds
                            .claim(&hm, admission.space())
                            .await
                            .map_err(|error| {
                                DockerError::ContainerCreationError(error.to_string())
                            })?;
                    }
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

    Ok(builder)
}

/// workspace 这类数据目录落在可回收的日志根下面：拒绝。清理器回收日志根下面过期且没有被
/// 挂载的目录，数据目录停用一阵就会被当成日志删掉。
///
/// 判不出在不在的不拒绝，返回 `true`：调用方把它的 bind 源登记到创建结束
async fn check_data_mount(
    admission: &LogDomainAdmission<'_>,
    container_path: &std::path::Path,
    host_path: &std::path::Path,
) -> crate::DockerResult<bool> {
    let what = format!("Data mount {}", container_path.display());
    let source = host_path.to_string_lossy();
    let verdict = admission.classify(&what, &source).await;
    if verdict == LogDomainVerdict::Inside {
        return Err(DockerError::ConfigurationError(format!(
            "{what} binds {source}, which lies under the reclaimable log root; the log cleaner would reclaim it. Move the data directory or cleanup_config.log_cleanup.log_dir"
        )));
    }
    Ok(matches!(verdict, LogDomainVerdict::Unknown(_)))
}

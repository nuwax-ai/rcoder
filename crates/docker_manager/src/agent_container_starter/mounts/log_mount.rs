//! 日志挂载：容器内路径带 `container-logs` 的那一条，它的宿主目录同时是容器内
//! `/app/container-logs` 的 bind 源。
//!
//! 目录一次性使用：每个容器绑定的都是启动它时独占新建的目录（见 `crate::log_directory`），
//! 已存在的同名目录不复用。日志清理器据此只凭"目录已过期、没有容器挂着它"回收，不必和
//! 这里的挂载准备互斥——清理器手里的候选都是旧目录。

use std::collections::HashMap;

use tracing::warn;

use super::templates::{directory_visible_to_this_process, path_after_resolved_base};
use crate::DockerError;
use shared_types::ServiceMountConfig;

/// 日志目录名在 `host_path` 模板里的变量
const LOG_DIR_NAME_VARIABLE: &str = "{log_dir_name}";

/// 这条挂载是不是容器的日志目录：容器内的路径带 `container-logs`
pub(super) fn is_log_mount(mount_config: &ServiceMountConfig) -> bool {
    mount_config.container_path.contains("container-logs")
}

/// 日志挂载必须绑定 `{log_dir_name}` 目录。绑别的目录等于让新容器复用一个已经存在的目录，
/// 它可能已经过期、正被日志清理器回收
pub(super) fn require_new_log_directory(
    mount_config: &ServiceMountConfig,
) -> crate::DockerResult<()> {
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
pub(super) async fn create_log_directory(
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
    // 宿主机上的完整路径：deploy-host 形态直接在它上面建目录
    let host_path = format!("{resolved_base}{suffix}");
    let create_path = directory_visible_to_this_process(&host_path, resolve_from, &suffix);
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

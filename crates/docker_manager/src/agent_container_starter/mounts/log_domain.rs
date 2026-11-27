//! 可回收日志域的准入：创建容器时，哪些 bind 源必须是为本容器新建的目录。
//!
//! 日志清理器回收**可回收的日志根**（`DockerManagerConfig::log_reclaim_root`，和清理器的
//! `cleanup_config.log_cleanup.log_dir` 是同一个目录）下面过期且没有被挂载的目录。所以指向这个
//! 域的 bind 源，不管容器内的挂载点叫什么，都必须是为本容器独占新建的目录（见
//! `crate::log_directory`）：绑定一个已有的目录，就可能和回收撞上。
//!
//! 在不在日志根里由 `host_path_space::LogDomain` 判定，用的是清理器那个路径空间：词法归一化
//! 之后比一次，解析符号链接之后再比一次。判得出来是什么就按什么执行。
//!
//! 判不出来时（日志根定位不完整、这条源的真实位置求不出来、取不到路径空间）**不拒绝创建
//! 容器**——那会让一个环境问题变成所有容器都起不来。判不出来的源照常挂载，登记到创建结束。
//! 清理器对它同样判不出来，而它确认不了就不删。
//!
//! 拒绝的只有确定的配置错误：只读挂载落在日志根里、模板拿不到独占目录、数据目录落在日志根
//! 里、新目录建不出来。建不出新目录就报错，不把首选路径原样交给 Docker：那样绑定到的可能是
//! 一个已有的目录，而普通创建用的 `HostConfig.Mounts` 在路径缺失时也不会补建。
//!
//! builder 重启出替代容器时是同一条规则（`runtime::docker_builder_restart::log_directory`）。

use std::collections::HashMap;
use std::path::Path;

use super::templates::{directory_visible_to_this_process, path_after_resolved_base};
use crate::DockerError;
use crate::log_directory::create_fresh_directory;
use shared_types::ServiceMountConfig;

/// 日志目录名在 `host_path` 模板里的变量
const LOG_DIR_NAME_VARIABLE: &str = "{log_dir_name}";
/// 模板里代表"解析后的基础路径"的变量
const RESOLVED_PATH_VARIABLE: &str = "{resolved_path}";

/// 这条挂载要不要告诉容器它的日志目录在哪（容器内的路径带 `container-logs`）。只管环境变量，
/// 不决定准入——准入看源路径
pub(super) fn sets_log_env(mount_config: &ServiceMountConfig) -> bool {
    mount_config.container_path.contains("container-logs")
}

/// 这条挂载指向可回收日志域里的一个目录：给本容器独占新建它，返回最终用上的日志目录名。
///
/// 建不出来就报错。只读的挂载也报错：RCoder 不给它建目录，它绑到的只能是已有的目录。
pub(super) async fn create_reclaimable_directory(
    mount_config: &ServiceMountConfig,
    variables: &HashMap<String, String>,
) -> crate::DockerResult<String> {
    let mount = &mount_config.container_path;
    if mount_config.read_only {
        return Err(DockerError::ConfigurationError(format!(
            "Mount {mount} binds a directory under the reclaimable log root; it must be writable, because RCoder creates a new directory there for every container"
        )));
    }
    // 模板只能是 {resolved_path}/…/{log_dir_name}：最后一级整个就是日志目录名，建出来的目录名
    // 才能原样填回模板
    let last_component_is_name =
        mount_config.host_path.rsplit('/').next() == Some(LOG_DIR_NAME_VARIABLE);
    if !mount_config.host_path.starts_with(RESOLVED_PATH_VARIABLE) || !last_component_is_name {
        return Err(DockerError::ConfigurationError(format!(
            "Mount {mount} binds a directory under the reclaimable log root; its host path {:?} must read {{resolved_path}}/…/{{log_dir_name}}, so that every container gets a directory of its own",
            mount_config.host_path
        )));
    }
    let (Some(resolve_from), Some(resolved_base)) = (
        mount_config.resolve_from.as_deref(),
        variables.get("resolved_path"),
    ) else {
        return Err(DockerError::ConfigurationError(format!(
            "Mount {mount} binds a directory under the reclaimable log root, but its base path cannot be resolved; configure resolve_from"
        )));
    };
    let suffix = path_after_resolved_base(mount_config, variables);
    // 宿主机上的完整路径：deploy-host 形态直接在它上面建目录
    let host_path = format!("{resolved_base}{suffix}");
    let create_path = directory_visible_to_this_process(&host_path, resolve_from, &suffix);
    let (Some(parent), Some(preferred)) = (
        Path::new(&create_path).parent(),
        Path::new(&create_path)
            .file_name()
            .and_then(|name| name.to_str()),
    ) else {
        return Err(DockerError::ConfigurationError(format!(
            "Mount {mount} has no log directory to create: {create_path:?}"
        )));
    };
    match create_fresh_directory(parent, preferred).await {
        Ok(created) => Ok(created
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| preferred.to_owned())),
        Err(error) => Err(DockerError::ContainerCreationError(format!(
            "Cannot create a new log directory for mount {mount}: {error} ({})",
            parent.display()
        ))),
    }
}

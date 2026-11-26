//! agent 容器挂载构建（从 agent_container_starter.rs 拆出）。
//!
//! [`apply_auto_mounts`] 分两步：
//!
//! - [`workspace`]：workspace 隔离挂载 auto-inject（service_type/隔离级别决定挂载策略）；
//! - [`config`]：config mounts 解析（冲突检测/宿主目录预创建/日志挂载 env），模板变量替换和
//!   建目录路径的算法在 [`templates`]。
//!
//! 日志挂载（容器内路径带 `container-logs`）的目录是一次性的：每个容器绑定的都是启动它
//! 时独占新建的目录，已存在的同名目录不复用（见 `crate::log_directory`），规则在
//! [`log_mount`]。日志清理器据此不必和这里的挂载准备互斥。

mod config;
mod log_mount;
mod templates;
mod workspace;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::container_builder::ContainerConfigBuilder;
use shared_types::{ServiceImageConfig, ServiceType};

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

/// 给 agent 容器加上挂载：先自动注入 workspace，再处理配置里的挂载点
pub(super) async fn apply_auto_mounts(
    builder: ContainerConfigBuilder,
    ctx: &MountContext<'_>,
    lookup: &dyn HostPathLookup,
) -> crate::DockerResult<ContainerConfigBuilder> {
    // 记录 auto-inject 已添加的 container_path，用于后续冲突检测
    let mut auto_injected_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let builder =
        workspace::inject_workspace_mounts(builder, ctx, lookup, &mut auto_injected_paths).await?;
    config::add_config_mounts(builder, ctx, lookup, &auto_injected_paths).await
}

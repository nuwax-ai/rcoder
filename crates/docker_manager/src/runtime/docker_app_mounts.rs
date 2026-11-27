//! Docker Userapp 运行容器的 prod 挂载组装（从 docker_app_runtime.rs 拆出）。
//!
//! prod 四目录压平挂载（与 dev builder 完全同构）：宿主
//! `{userapp 根}/prod/{user_id}/` 下四目录（`{app_id}`、`data/{app_id}`、
//! `logs/{app_id}`、`agent-store/{app_id}`；布局单一事实源
//! [`shared_types::paths::userapp_prod_subpaths`]）→ 容器内
//! `/home/user/{app_id}`（workspace=发布代码根）、`/home/user/data`、
//! `/home/user/logs`、`/home/user/.agent-store`——app 间数据完全隔离。
//! 锚点反解失败 fail fast：静默降级等于 PG/制品落容器 overlay，容器删除即丢
//! （与 builder 卷同款 fail-fast 语义）。
//!
//! 四个目录都是数据目录，不能落在可回收的日志根里（日志根配得和 prod 目录树重叠时）：组装
//! 时先判定，重叠就拒绝创建应用，目录也不建；判不出来的只告警。
//!
//! # 应用容器的挂载守卫
//!
//! 首次创建、更新、换镜像重建都经 [`app_mount_guard`]：数据目录落在日志根里就拒绝；这些 bind
//! 源登记到新容器建出来。更新和重建是先删旧容器再建，中间这一段数据目录不在任何容器的挂载
//! 里，日志清理器会把它们当成没人用——守卫要在删旧容器之前拿到，登记才接得上。重建的挂载
//! 是从旧容器的 `HostConfig` 原样抄的，不过组装这一步，所以守卫看的是"实际要绑什么"。
//!
//! 四个目录都是数据目录，不能落在可回收的日志根里（日志根配得和 prod 目录树重叠时）：
//! 组装时先判定，重叠或判不出来都拒绝创建应用，目录也不建。

use std::path::{Path, PathBuf};

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};

use super::docker_runtime::DockerRuntime;
use super::docker_runtime::host_path_space::{LogDomainAdmission, LogDomainVerdict, bind_source};
use crate::log_directory::PendingBinds;

/// 组装 prod 四目录 bind 挂载（含宿主目录预创建）。
///
/// 预创建走 rcoder 容器内锚点路径 mkdir（经 compose bind 双向同步宿主，daemon
/// 即刻可见 bind 源）；失败仅 warn 降级（Docker daemon 对缺失 bind 源会自动
/// mkdir 兜底，root 属主，风险低）。
pub(super) async fn build_prod_flat_mounts(
    runtime: &DockerRuntime,
    app_id: &str,
    _user_id: Option<&str>,
) -> ContainerRuntimeResult<Vec<Mount>> {
    let host_root = crate::path::resolve_container_path_to_host(Path::new(
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
    ))
    .await
    .map_err(|e| {
        ContainerRuntimeError::DockerError(format!(
            "Userapp prod volume host path resolve failed (rcoder 容器需挂载 userapp-workspace 锚点): {e}"
        ))
    })?;
    // 容器形态：经容器内锚点路径创建（compose bind 双向同步宿主）；
    // deploy-host：直接在 bind 源（host_root 子路径）创建。
    #[cfg(feature = "deploy-host")]
    let precreate_root: &Path = if shared_types::is_deploy_host() {
        &host_root
    } else {
        Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
    };
    #[cfg(not(feature = "deploy-host"))]
    let precreate_root: &Path = Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    prod_flat_mounts(runtime, &host_root, precreate_root, app_id).await
}

/// 应用容器要绑定的全部 bind：`HostConfig.Mounts` 里类型为 bind 的，加上旧式的 `Binds`。
/// 每项是（宿主机源，容器内挂载点）
pub(super) fn bind_mounts(host_config: Option<&HostConfig>) -> Vec<(String, String)> {
    let mut mounts = Vec::new();
    let Some(host_config) = host_config else {
        return mounts;
    };
    for mount in host_config.mounts.iter().flatten() {
        if mount.typ != Some(MountType::BIND) {
            continue;
        }
        if let (Some(source), Some(target)) = (mount.source.as_deref(), mount.target.as_deref())
            && !source.is_empty()
        {
            mounts.push((source.to_owned(), target.to_owned()));
        }
    }
    for bind in host_config.binds.iter().flatten() {
        // 和 Docker 的切分一致：以 `/` 开头的才是宿主机路径，取到第一个冒号为止
        let Some(source) = bind_source(bind) else {
            continue;
        };
        mounts.push((
            source.to_owned(),
            bind.split(':').nth(1).unwrap_or_default().to_owned(),
        ));
    }
    mounts
}

/// 应用容器的挂载守卫：数据目录落在可回收的日志根里就拒绝；否则登记这些 bind 源，调用方在
/// 新容器建出来之前一直拿着（[`PendingBinds::until_settled`]）。
///
/// 见模块说明。判不出来的不拒绝，照样登记。
pub(super) async fn app_mount_guard(
    runtime: &DockerRuntime,
    config: &ContainerCreateBody,
) -> ContainerRuntimeResult<PendingBinds> {
    let mounts = bind_mounts(config.host_config.as_ref());
    let sources: Vec<PathBuf> = mounts
        .iter()
        .map(|(source, _)| PathBuf::from(source))
        .collect();
    let targets: Vec<String> = mounts.iter().map(|(_, target)| target.clone()).collect();
    refuse_prod_mounts_in_log_domain(runtime, &sources, &targets).await?;
    let mut pending = PendingBinds::default();
    for source in sources {
        pending.claim(&source);
    }
    Ok(pending)
}

/// `host_root` 是 prod 目录树在宿主机上的根；`precreate_root` 是这棵树在本进程里的路径
async fn prod_flat_mounts(
    runtime: &DockerRuntime,
    host_root: &Path,
    precreate_root: &Path,
    app_id: &str,
) -> ContainerRuntimeResult<Vec<Mount>> {
    let subs = shared_types::paths::userapp_prod_subpaths(app_id);
    let targets = prod_flat_container_paths(app_id);
    let sources: Vec<PathBuf> = subs.iter().map(|rel| host_root.join(rel)).collect();
    // 先判定再建目录：被拒绝的布局不会在日志根里留下任何东西
    refuse_prod_mounts_in_log_domain(runtime, &sources, &targets).await?;
    let mut mounts = Vec::with_capacity(4);
    for ((rel, target), host_path) in subs.iter().zip(targets).zip(&sources) {
        let precreate = precreate_root.join(rel);
        if let Err(e) = tokio::fs::create_dir_all(&precreate).await {
            tracing::warn!(
                "[DOCKER_APP] create prod dir {} failed (continuing): {e}",
                precreate.display()
            );
        }
        tracing::info!(
            "[DOCKER_APP] prod flat mount: {} -> {target}",
            host_path.display()
        );
        mounts.push(Mount {
            target: Some(target),
            source: Some(host_path.to_string_lossy().to_string()),
            typ: Some(MountType::BIND),
            ..Default::default()
        });
    }
    Ok(mounts)
}

/// prod 挂载都是数据目录，不能落在可回收的日志根里：清理器会把那里过期且没被挂载的目录当
/// 日志回收。日志根配得和 prod 目录树重叠时，创建应用就直接拒绝，不等到数据被删。
///
/// 在不在日志根里用的是清理器那个路径空间。判不出来的不拒绝（那会让应用发布不出去），只
/// 告警；取不到路径空间、日志根定位不完整也是只告警。
async fn refuse_prod_mounts_in_log_domain(
    runtime: &DockerRuntime,
    sources: &[PathBuf],
    targets: &[String],
) -> ContainerRuntimeResult<()> {
    let admission = LogDomainAdmission::locate(
        &runtime.inner,
        &runtime.log_path_resolver,
        &runtime.inner.config.log_reclaim_root,
    )
    .await;
    admission.warn_if_limited("Assemble application data mounts");
    for (source, target) in sources.iter().zip(targets) {
        let source = source.to_string_lossy();
        let verdict = admission
            .classify(&format!("Application data directory {target}"), &source)
            .await;
        if verdict == LogDomainVerdict::Inside {
            return Err(ContainerRuntimeError::ConfigurationError(format!(
                "The application's data directory {target} binds {source}, which lies under the reclaimable log root; the log cleaner would reclaim it. Move cleanup_config.log_cleanup.log_dir"
            )));
        }
    }
    Ok(())
}

/// prod 压平挂载的容器内路径四元组（与 [`shared_types::paths::userapp_prod_subpaths`]
/// 段序一一配对）：workspace=`/home/user/{app_id}`（发布代码根）、
/// data/logs/agent-store 三常量——与 dev builder 容器内布局完全同图。
pub(crate) fn prod_flat_container_paths(app_id: &str) -> [String; 4] {
    [
        format!("{}/{}", shared_types::paths::USERAPP_DEV_HOME, app_id),
        shared_types::paths::USERAPP_DEV_DATA.to_string(),
        shared_types::paths::USERAPP_DEV_LOGS.to_string(),
        shared_types::paths::USERAPP_DEV_AGENT_STORE.to_string(),
    ]
}

#[cfg(test)]
mod tests;

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
//! 四个目录都是数据目录，不能落在可回收的日志根里（日志根配得和 prod 目录树重叠时）：
//! 组装时先判定，重叠或判不出来都拒绝创建应用，目录也不建。

use std::path::{Path, PathBuf};

use bollard::models::{Mount, MountType};
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};

use super::docker_runtime::DockerRuntime;
use super::docker_runtime::host_path_space::{LogDomainAdmission, LogDomainVerdict};

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
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn prod_flat_mounts_pair_with_layout_source() {
        let subs = shared_types::paths::userapp_prod_subpaths("a1");
        let targets = prod_flat_container_paths("a1");
        // 段序配对：workspace/data/logs/agent-store → /home/user 下四挂载点
        assert_eq!(
            targets,
            [
                "/home/user/a1".to_string(),
                "/home/user/data".to_string(),
                "/home/user/logs".to_string(),
                "/home/user/.agent-store".to_string(),
            ]
        );
        assert_eq!(subs[0], "prod/userapp/a1");
        assert!(subs[1].ends_with("data/a1") && targets[1].ends_with("/data"));
        assert!(subs[2].ends_with("logs/a1") && targets[2].ends_with("/logs"));
        assert!(subs[3].ends_with("agent-store/a1") && targets[3].ends_with("/.agent-store"));
        // data 段兼容视图与布局事实源一致（清理链存量调用方依赖）
        assert_eq!(
            subs[1],
            shared_types::paths::userapp_prod_data_subpath("a1")
        );
    }

    /// 一个运行时：日志根是 `reclaim_root`，挂载表只有一条（`covered` 挂在它自己的路径上）
    fn runtime(reclaim_root: PathBuf, covered: &Path) -> DockerRuntime {
        let (actor, containers) = crate::ContainerStateActor::new();
        tokio::spawn(actor.run());
        let manager = Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                "http://127.0.0.1:9",
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig {
                log_reclaim_root: reclaim_root,
                ..Default::default()
            },
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        });
        let resolver = crate::HostPathResolver::from_mount_table(vec![(
            covered.to_path_buf(),
            covered.to_path_buf(),
        )])
        .unwrap();
        DockerRuntime::with_log_path_resolver(manager, resolver)
    }

    /// 日志根在另一棵目录树上（默认部署的样子）：prod 挂载不在它里面，照常组装并预创建。
    #[tokio::test]
    async fn prod_mounts_outside_the_log_root_are_allowed() {
        let covered = tempfile::tempdir().unwrap();

        let mounts = prod_flat_mounts(
            &runtime("/srv/logs/container".into(), covered.path()),
            covered.path(),
            covered.path(),
            "a1",
        )
        .await
        .unwrap();

        let subs = shared_types::paths::userapp_prod_subpaths("a1");
        assert_eq!(mounts.len(), 4);
        let sources: Vec<String> = subs
            .iter()
            .map(|rel| covered.path().join(rel).to_string_lossy().into_owned())
            .collect();
        let sources: Vec<&str> = sources.iter().map(String::as_str).collect();
        assert_eq!(
            mounts
                .iter()
                .filter_map(|mount| mount.source.as_deref())
                .collect::<Vec<_>>(),
            sources,
            "every prod directory is bind-mounted from the prod tree"
        );
        for source in &sources {
            assert!(
                std::fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()),
                "the bind source {source} is created before the container"
            );
        }
    }

    /// 日志根配到了 prod 目录树上面：组装时拒绝，一个目录都不建。
    #[tokio::test]
    async fn prod_mounts_under_the_log_root_are_refused() {
        let covered = tempfile::tempdir().unwrap();

        let error = prod_flat_mounts(
            &runtime(covered.path().to_path_buf(), covered.path()),
            covered.path(),
            covered.path(),
            "a1",
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&error, ContainerRuntimeError::ConfigurationError(message)
                if message.contains("data directory /home/user/a1 binds")
                    && message.contains("which lies under the reclaimable log root")),
            "{error}"
        );
        assert!(
            !covered.path().join("prod").exists(),
            "a refused layout must not leave directories behind"
        );
    }
    /// 判不出数据目录在不在日志根里（这里日志根是相对路径，定位不了）：不拒绝创建应用。
    /// 拒绝的话，一个环境问题会让应用发布不出去
    #[tokio::test]
    async fn prod_mounts_that_cannot_be_classified_are_allowed() {
        let covered = tempfile::tempdir().unwrap();

        let mounts = prod_flat_mounts(
            &runtime("logs/container".into(), covered.path()),
            covered.path(),
            covered.path(),
            "a1",
        )
        .await
        .unwrap_or_else(|error| {
            panic!("an unlocatable log root must not block the application: {error}")
        });

        assert_eq!(mounts.len(), 4);
    }
}

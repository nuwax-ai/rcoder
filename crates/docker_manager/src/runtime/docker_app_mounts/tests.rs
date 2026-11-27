//! prod 四挂载的组装和挂载守卫：bind 的两种写法都认；数据目录落在日志根里就拒绝；其余 bind
//! 源登记到新容器建出来，判不出来的照样登记。

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
fn bind(source: &str, target: &str) -> Mount {
    Mount {
        target: Some(target.into()),
        source: Some(source.into()),
        typ: Some(MountType::BIND),
        ..Default::default()
    }
}

fn container_body(mounts: Vec<Mount>, binds: Vec<String>) -> ContainerCreateBody {
    ContainerCreateBody {
        host_config: Some(HostConfig {
            mounts: Some(mounts),
            binds: Some(binds),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items
}

/// 两种写法都认：`Mounts` 里类型为 bind 的，和旧式的 `Binds`。命名卷、Windows 盘符不是宿主机路径
#[test]
fn bind_mounts_come_from_both_mount_forms() {
    let body = container_body(
        vec![
            bind("/srv/a1", "/home/user/a1"),
            Mount {
                target: Some("/home/user/data".into()),
                source: Some("named-volume".into()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            },
        ],
        vec![
            "/srv/data:/home/user/data".into(),
            "/srv/logs:/home/user/logs:rw,z".into(),
            "named:/data".into(),
            "C:\\data:/data".into(),
        ],
    );

    let mounts = bind_mounts(body.host_config.as_ref());
    assert_eq!(
        sorted(mounts.clone()),
        sorted(vec![
            ("/srv/a1".to_owned(), "/home/user/a1".to_owned()),
            ("/srv/data".to_owned(), "/home/user/data".to_owned()),
            ("/srv/logs".to_owned(), "/home/user/logs".to_owned()),
        ])
    );
}

/// 数据目录落在日志根里：守卫拒绝，调用方据此不删旧容器，也不留下任何登记
#[tokio::test]
async fn the_guard_refuses_data_directories_under_the_log_root() {
    let covered = tempfile::tempdir().unwrap();
    let mounts = vec![bind(
        &covered.path().join("prod/userapp/a1").to_string_lossy(),
        "/home/user/a1",
    )];
    let body = container_body(mounts, Vec::new());

    let error = app_mount_guard(
        &runtime(covered.path().to_path_buf(), covered.path()),
        &body,
    )
    .await
    .expect_err("data directories under the log root must be refused");

    assert!(
        matches!(&error, ContainerRuntimeError::ConfigurationError(message) if message.contains("data directory /home/user/a1 binds")),
        "{error}"
    );
    assert!(
        crate::log_directory::pending_bind_sources().is_empty(),
        "a refused application leaves nothing registered"
    );
}

/// 守卫登记的是容器实际要绑的全部 bind 源，不管挂载是组装出来的还是从旧容器抄的。拿到
/// 守卫的调用方在新容器建出来之前不放手：更新和重建是先删旧容器再建。
#[tokio::test]
async fn the_guard_registers_every_bind_the_container_will_get() {
    let covered = tempfile::tempdir().unwrap();
    let (a1, data) = (
        covered.path().join("prod/userapp/a1"),
        covered.path().join("prod/data/a1"),
    );
    let body = container_body(
        vec![bind(&a1.to_string_lossy(), "/home/user/a1")],
        vec![format!("{}:/home/user/data", data.to_string_lossy())],
    );

    let guard = app_mount_guard(
        &runtime("/srv/logs/container".into(), covered.path()),
        &body,
    )
    .await
    .unwrap();

    assert_eq!(
        sorted(crate::log_directory::pending_bind_sources()),
        sorted(vec![a1, data.clone()])
    );
    drop(guard);
    assert!(crate::log_directory::pending_bind_sources().is_empty());
}

/// 判不出来（这里日志根是相对路径，定位不了）：不拒绝，照样登记
#[tokio::test]
async fn the_guard_registers_what_it_cannot_classify() {
    let covered = tempfile::tempdir().unwrap();
    let a1 = covered.path().join("prod/userapp/a1");
    let body = container_body(
        vec![bind(&a1.to_string_lossy(), "/home/user/a1")],
        Vec::new(),
    );

    let guard = app_mount_guard(&runtime("logs/container".into(), covered.path()), &body)
        .await
        .unwrap_or_else(|error| {
            panic!("an unlocatable log root must not block the application: {error}")
        });

    assert_eq!(crate::log_directory::pending_bind_sources(), [a1]);
    drop(guard);
    assert!(crate::log_directory::pending_bind_sources().is_empty());
}

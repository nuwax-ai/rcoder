//! 判不出来时不拒绝创建容器：bind 源的真实位置求不出来、日志根只定出词法位置、取不到路径
//! 空间。挂载照常加上，判不出来的 bind 源登记到创建结束——清理器对它同样判不出来，而它
//! 确认不了就不删。

use super::*;
#[cfg(not(feature = "deploy-host"))]
use std::sync::Arc;

/// workspace 的宿主机路径判不出在不在日志根里（经过一个指向自己的符号链接）：不拒绝。数据
/// 挂载照常加上，bind 源登记到创建结束
#[tokio::test]
async fn workspace_data_that_cannot_be_classified_is_mounted_as_usual_and_registered() {
    let layout = Layout::new();
    std::fs::create_dir_all(layout.host_assets_root()).unwrap();
    std::os::unix::fs::symlink("loop", layout.host_assets_root().join("loop")).unwrap();

    let (config, pending) = apply_with_workspace(
        &layout,
        Vec::new(),
        Some(&layout.host_assets_root().join("loop")),
    )
    .await
    .unwrap();

    let workspace = config
        .extra_mounts
        .iter()
        .find(|mount| mount.container_path == "/home/user")
        .unwrap();
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from(&workspace.host_path)],
        "a data mount that cannot be classified must be registered until the container is created"
    );
    drop(pending);
    assert!(pending_bind_sources().is_empty());
}

/// 判不出一条 bind 源在不在日志根里（这里是一个自己指向自己的链接）：不拒绝创建容器。这条
/// 挂载照常加上，它的源登记到创建结束——清理器定不了它的位置，登记着就整轮不删
#[tokio::test]
async fn a_bind_source_that_cannot_be_classified_is_mounted_as_usual_and_registered() {
    let layout = Layout::new();
    std::fs::create_dir_all(layout.host_assets_root()).unwrap();
    std::os::unix::fs::symlink("loop", layout.host_assets_root().join("loop")).unwrap();
    let through_loop = mount(
        "/app/notes",
        "{resolved_path}/loop/shared",
        false,
        Some(&layout.assets_resolve_from()),
    );

    let (config, pending) = apply(&layout, vec![through_loop]).await.unwrap();

    let mounted = only_mount(&config);
    assert_eq!(mounted.container_path, "/app/notes");
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from(&mounted.host_path)],
        "a mount that cannot be classified must be registered until the container is created"
    );
    drop(pending);
    assert!(pending_bind_sources().is_empty());
}

/// 日志根的真实位置求不出来（这里是一个指向自己的符号链接）：不拒绝创建容器。词法上不在
/// 它下面的挂载判不出来在不在，照常挂载，源登记到创建结束
#[tokio::test]
async fn a_log_root_that_cannot_be_located_does_not_block_the_container() {
    let layout = Layout::new();
    std::fs::create_dir_all(layout.host_assets_root()).unwrap();
    std::os::unix::fs::symlink("container", layout.host_log_root()).unwrap();
    let shared = mount(
        "/app/shared",
        "{resolved_path}/shared",
        false,
        Some(&layout.assets_resolve_from()),
    );

    let (config, pending) = apply(&layout, vec![shared]).await.unwrap();

    let mounted = only_mount(&config);
    assert_eq!(mounted.container_path, "/app/shared");
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from(&mounted.host_path)],
        "nothing can be classified against an unlocatable root; the mount is registered until the container is created"
    );
    drop(pending);
    assert!(pending_bind_sources().is_empty());
}

/// 取不到路径空间：不拒绝创建容器。挂载照常加上，没有哪条判得出在不在日志根里，全部登记
/// 到创建结束
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn an_unavailable_mount_table_does_not_block_the_container() {
    let (actor, containers) = crate::ContainerStateActor::new();
    tokio::spawn(actor.run());
    // 一个没人监听的端口：自检必定失败
    let unreachable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = unreachable.local_addr().unwrap();
    drop(unreachable);
    let layout = Layout::new();
    let manager = DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig {
            log_reclaim_root: layout.host_log_root(),
            ..Default::default()
        },
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    };
    // 配置里就是标准的日志挂载。路径换算是假的、能用；判不出它在不在日志根里的是路径空间
    let service_config: ServiceImageConfig = serde_json::from_value(serde_json::json!({
        "service_type": "computer-agent-runner",
        "enabled": true,
        "mounts": [log_mount(&layout)],
    }))
    .unwrap();
    let variables = HashMap::new();
    let log_reclaim_root = layout.host_log_root();
    let ctx = mount_context(&log_reclaim_root, &service_config, &variables);
    let lookup = FakeLookup {
        mappings: vec![(layout.visible_log_root(), layout.host_log_root())],
    };

    let applied = prepare_auto_mounts(
        &manager,
        ContainerConfigBuilder::new("user_42"),
        &ctx,
        &lookup,
    )
    .await
    .unwrap_or_else(|error| {
        panic!("an unavailable mount table must not block the container: {error:?}")
    });
    let config = applied.builder.build().unwrap();

    let mounted = only_mount(&config);
    assert_eq!(mounted.container_path, "/app/container-logs");
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from(&mounted.host_path)],
        "a mount that cannot be classified must be registered until the container is created"
    );
    drop(applied.pending_binds);
    assert!(pending_bind_sources().is_empty());
}

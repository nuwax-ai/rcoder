//! 创建容器时的日志挂载记录（容器 label `rcoder.io/log-mounts`）：之后重启这个容器，只有记录里的
//! 挂载才换成新目录。所以只记 RCoder 真的为它独占新建了目录的那几条。

use super::*;
use crate::log_directory::{LOG_MOUNTS_LABEL, decode_log_mounts};

/// label 上记下的日志挂载：（容器内挂载点，RCoder 新建的目录），按挂载点排好
fn recorded(config: &crate::DockerContainerConfig) -> Vec<(String, String)> {
    let label = config
        .labels
        .as_ref()
        .and_then(|labels| labels.get(LOG_MOUNTS_LABEL));
    let mut records: Vec<(String, String)> =
        decode_log_mounts(label.map(String::as_str).unwrap_or_default())
            .unwrap_or_default()
            .into_iter()
            .map(|record| (record.target, record.source))
            .collect();
    records.sort();
    records
}

/// 指向日志根的挂载每条都记：挂载点是它实际绑定的那个，源是 RCoder 为它新建的那个目录
#[tokio::test]
async fn every_directory_rcoder_created_is_recorded_with_the_mount_it_serves() {
    let layout = Layout::new();
    let audit = mount(
        AUDIT_TARGET,
        "{resolved_path}/{log_dir_name}",
        false,
        Some(&layout.log_resolve_from()),
    );

    let (config, _pending) = apply(&layout, vec![audit, log_mount(&layout)])
        .await
        .unwrap();

    let mut bound: Vec<(String, String)> = config
        .extra_mounts
        .iter()
        .map(|mount| (mount.container_path.clone(), mount.host_path.clone()))
        .collect();
    bound.sort();
    assert_eq!(
        recorded(&config),
        bound,
        "what is bound is what is recorded"
    );
    for (_, source) in &bound {
        let name = Path::new(source).file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with(CONTAINER),
            "a directory RCoder created: {name}"
        );
        assert!(layout.host_log_root().join(name.as_ref()).is_dir());
    }
}

/// 日志根之外的挂载不是 RCoder 新建的日志目录：不进记录，一条都没有时连 label 都不写
#[tokio::test]
async fn a_mount_outside_the_log_root_is_not_recorded() {
    let layout = Layout::new();
    std::fs::create_dir_all(layout.host_assets_root().join("wallpaper")).unwrap();

    let (config, _pending) = apply(
        &layout,
        vec![mount(
            "/app/wallpaper",
            "{resolved_path}/wallpaper",
            false,
            Some(&layout.assets_resolve_from()),
        )],
    )
    .await
    .unwrap();

    assert!(recorded(&config).is_empty());
    assert!(
        config
            .labels
            .as_ref()
            .is_none_or(|labels| !labels.contains_key(LOG_MOUNTS_LABEL)),
        "no log mount, no label: {:?}",
        config.labels
    );
}

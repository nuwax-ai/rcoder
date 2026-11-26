//! `apply_auto_mounts` 的测试：用临时目录和假的路径换算，不连 Docker。
//!
//! 带不带 `deploy-host` 特性各编译一次，两种配置下这些用例都要成立：建目录走哪个路径在
//! 那里分叉，所以目录布局按形态各留了一条用例。

use super::*;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;
use shared_types::ServiceMountConfig;

const CONTAINER: &str = "computer-agent-runner-user_42";

/// 一次测试用的目录布局：`host` 是 daemon 看到的日志总根，`visible` 是同一个目录在 RCoder
/// 进程里的样子。
///
/// [`Layout::new`] 让两者是同一个路径，带不带 `deploy-host` 特性都成立。容器形态下两者是
/// 两个路径，见 [`Layout::with_separate_views`]。
struct Layout {
    root: tempfile::TempDir,
    /// `visible` 是不是另一个路径（指向 `host` 的符号链接）
    separate_views: bool,
}

impl Layout {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            separate_views: false,
        }
    }

    /// 容器形态的布局：`visible/container` 是指向 `host/container` 的符号链接
    fn with_separate_views() -> Self {
        let layout = Self {
            root: tempfile::tempdir().unwrap(),
            separate_views: true,
        };
        std::fs::create_dir_all(layout.host_log_root()).unwrap();
        std::fs::create_dir(layout.root.path().join("visible")).unwrap();
        std::os::unix::fs::symlink("../host/container", layout.visible_log_root()).unwrap();
        layout
    }

    fn host_log_root(&self) -> PathBuf {
        if self.separate_views {
            self.root.path().join("host/container")
        } else {
            self.root.path().join("container")
        }
    }

    fn visible_log_root(&self) -> PathBuf {
        if self.separate_views {
            self.root.path().join("visible/container")
        } else {
            self.host_log_root()
        }
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

/// CONTAINER_LOGS_DIR 写的是实际挂载点（变量已替换），不是配置里的模板：builder 重启靠它
/// 认出归档里的日志挂载。
#[tokio::test]
async fn the_log_directory_env_names_the_path_that_is_actually_mounted() {
    let layout = Layout::new();
    let templated = mount(
        "/app/container-logs/{timestamp}",
        "{resolved_path}/{log_dir_name}",
        false,
        &layout.visible_log_root(),
    );

    let config = apply(&layout, vec![templated]).await.unwrap();

    let mount = only_mount(&config);
    assert_ne!(mount.container_path, "/app/container-logs/{timestamp}");
    assert!(mount.container_path.starts_with("/app/container-logs/"));
    assert_eq!(
        config
            .env_vars
            .get("CONTAINER_LOGS_DIR")
            .map(String::as_str),
        Some(mount.container_path.as_str()),
        "the container is told where its logs really are"
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

/// 容器形态：目录经 RCoder 进程里的路径建出来——它和宿主机路径不是同一个字符串；bind 用
/// 的是宿主机路径。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn in_container_form_the_directory_is_created_through_the_process_path() {
    let layout = Layout::with_separate_views();

    let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

    let mount = only_mount(&config);
    let name = file_name(&mount.host_path);
    assert!(
        mount
            .host_path
            .starts_with(layout.host_log_root().to_str().unwrap()),
        "the bind is the host path: {}",
        mount.host_path
    );
    assert!(layout.host_log_root().join(name).is_dir());
    assert!(layout.visible_log_root().join(name).is_dir());
    assert_eq!(entries(&layout.host_log_root()), [name.to_owned()]);
}

/// deploy-host 形态：RCoder 进程就是宿主机进程，目录直接建在 bind 源上。bind 源里面不再
/// 多出一层同名目录。
#[cfg(feature = "deploy-host")]
#[tokio::test]
async fn in_deploy_host_form_the_directory_is_created_on_the_bind_source_itself() {
    let layout = Layout::with_separate_views();

    let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

    let mount = only_mount(&config);
    let bound = layout.host_log_root().join(file_name(&mount.host_path));
    assert_eq!(mount.host_path, bound.to_str().unwrap());
    assert!(bound.is_dir());
    assert!(
        entries(&bound).is_empty(),
        "the bind source must not contain a nested directory of the same name"
    );
    assert_eq!(
        entries(&layout.host_log_root()),
        [bound.file_name().unwrap().to_str().unwrap().to_owned()]
    );
}

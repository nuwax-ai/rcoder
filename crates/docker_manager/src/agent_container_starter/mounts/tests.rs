//! `apply_auto_mounts` 的测试：用临时目录和假的路径换算，不连 Docker。
//!
//! 带不带 `deploy-host` 特性各编译一次，两种配置下这些用例都要成立：建目录走哪个路径在
//! 那里分叉，所以目录布局按形态各留了一条用例。
//!
//! 可回收的日志根按**源路径**准入：bind 源落在它下面的挂载，必须是为本容器独占新建的目录，
//! 否则拒绝创建容器；建好之后登记为正在准备挂载。

use super::*;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::DockerError;
use crate::container_builder::ContainerConfigBuilder;
use crate::log_directory::{PendingBinds, pending_bind_sources};
use shared_types::ServiceMountConfig;

const CONTAINER: &str = "computer-agent-runner-user_42";
/// 可回收的日志根（RCoder 进程里的路径）
const RECLAIM_ROOT: &str = "/app/logs/container";
/// 日志根下面的另一条日志挂载在容器内的路径
const AUDIT_TARGET: &str = "/app/audit-logs";
/// 不在日志根下面的另一块挂载（RCoder 进程里的路径）
const ASSETS_ROOT: &str = "/app/assets";
/// 工作区的解析路径
const WORKSPACE_ROOT: &str = "/app/computer-project-workspace";

/// 一次测试用的目录布局：`host` 是 daemon 看到的路径，`visible` 是同一个目录在 RCoder 进程里
/// 的样子。
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

    fn host_assets_root(&self) -> PathBuf {
        self.root.path().join("assets")
    }
}

/// 假的路径换算：只认登记过的几个路径，别的一律解析失败
struct FakeLookup {
    /// RCoder 进程里的路径 → 宿主机路径
    mappings: Vec<(PathBuf, PathBuf)>,
}

#[async_trait]
impl HostPathLookup for FakeLookup {
    async fn host_path(&self, container_path: &str) -> crate::DockerResult<PathBuf> {
        self.mappings
            .iter()
            .find(|(visible, _)| visible.to_str() == Some(container_path))
            .map(|(_, host)| host.clone())
            .ok_or_else(|| {
                DockerError::ConfigurationError(format!("no host path for {container_path}"))
            })
    }
}

fn mount(
    container_path: &str,
    host_path: &str,
    read_only: bool,
    resolve_from: Option<&str>,
) -> ServiceMountConfig {
    ServiceMountConfig {
        container_path: container_path.into(),
        host_path: host_path.into(),
        read_only,
        mount_type: "bind".into(),
        resolve_from: resolve_from.map(str::to_owned),
    }
}

/// 标准的日志挂载
fn log_mount(layout: &Layout) -> ServiceMountConfig {
    mount(
        "/app/container-logs",
        "{resolved_path}/{log_dir_name}",
        false,
        Some(layout.visible_log_root().to_str().unwrap()),
    )
}

/// 返回建好的配置和"正在准备挂载"的登记
async fn apply(
    layout: &Layout,
    mounts: Vec<ServiceMountConfig>,
) -> crate::DockerResult<(crate::DockerContainerConfig, PendingBinds)> {
    apply_with_workspace(layout, mounts, None).await
}

/// `workspace_host` 给出时，工作区的解析路径换算到它
async fn apply_with_workspace(
    layout: &Layout,
    mounts: Vec<ServiceMountConfig>,
    workspace_host: Option<&Path>,
) -> crate::DockerResult<(crate::DockerContainerConfig, PendingBinds)> {
    let service_config: ServiceImageConfig = serde_json::from_value(serde_json::json!({
        "service_type": "computer-agent-runner",
        "enabled": true,
        "mounts": mounts,
    }))
    .unwrap();
    let variables = HashMap::new();
    let ctx = MountContext {
        log_reclaim_root: Path::new(RECLAIM_ROOT),
        container_id: "user_42",
        container_prefix: "computer-agent-runner",
        service_config: &service_config,
        variables: &variables,
        workspace_resolution: WORKSPACE_ROOT,
        workspace_container: "/home/user",
        service_type: &ServiceType::ComputerAgentRunner,
        isolation_type: None,
        project_id: None,
        user_id: Some("user_42"),
        pod_id: None,
        tenant_id: None,
        space_id: None,
    };
    // 日志根有两个名字：配置里的 `log_reclaim_root`，和挂载的 `resolve_from`
    let mut mappings = vec![
        (PathBuf::from(RECLAIM_ROOT), layout.host_log_root()),
        (layout.visible_log_root(), layout.host_log_root()),
        (PathBuf::from(ASSETS_ROOT), layout.host_assets_root()),
    ];
    if let Some(host) = workspace_host {
        mappings.push((PathBuf::from(WORKSPACE_ROOT), host.to_path_buf()));
    }
    let applied = apply_auto_mounts(
        ContainerConfigBuilder::new("user_42"),
        &ctx,
        &FakeLookup { mappings },
    )
    .await?;
    let config = applied.builder.build()?;
    Ok((config, applied.pending_binds))
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

    let (config, _pending) = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

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

/// 日志目录建好之后、Docker 应答之前一直登记着：清理器把登记着的目录当作仍被挂载。
#[tokio::test]
async fn a_fresh_log_directory_stays_claimed_until_the_caller_lets_go() {
    let layout = Layout::new();

    let (config, pending) = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from(&only_mount(&config).host_path)]
    );
    drop(pending);
    assert!(
        pending_bind_sources().is_empty(),
        "once Docker has answered, the inventory alone protects the directory"
    );
}

#[tokio::test]
async fn an_existing_log_directory_is_never_bound_again() {
    let layout = Layout::new();
    take_upcoming_names(&layout, 2);

    let (config, _pending) = apply(&layout, vec![log_mount(&layout)]).await.unwrap();

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

/// 建不出新目录就建不出，不把首选路径原样交给 Docker：那样绑定到的可能是一个已有的目录。
#[tokio::test]
async fn a_log_directory_that_cannot_be_created_fails_the_container() {
    let layout = Layout::new();
    // 日志总根的位置被一个普通文件占着
    std::fs::write(layout.host_log_root(), b"not a directory").unwrap();

    let error = apply(&layout, vec![log_mount(&layout)]).await.unwrap_err();

    assert!(
        matches!(&error, DockerError::ContainerCreationError(message) if message.contains("Cannot create a new log directory")),
        "{error:?}"
    );
    assert!(
        pending_bind_sources().is_empty(),
        "nothing is handed to Docker, so nothing is claimed"
    );
}

/// 只读的日志挂载：RCoder 不给它建目录，它绑到的只能是已有的目录
#[tokio::test]
async fn a_read_only_mount_into_the_log_root_is_rejected() {
    let layout = Layout::new();
    let read_only = ServiceMountConfig {
        read_only: true,
        ..log_mount(&layout)
    };

    let error = apply(&layout, vec![read_only]).await.unwrap_err();

    assert!(
        matches!(&error, DockerError::ConfigurationError(message) if message.contains("must be writable")),
        "{error:?}"
    );
    assert!(
        !layout.host_log_root().exists(),
        "no directory is made for a rejected mount"
    );
}

/// 日志根下面的共享目录：清理器回收它下面过期且没被挂载的目录，不能往里绑
#[tokio::test]
async fn a_shared_directory_in_the_log_root_is_rejected() {
    let layout = Layout::new();
    let shared = mount(
        "/app/shared-logs",
        "{resolved_path}/shared",
        false,
        Some(layout.visible_log_root().to_str().unwrap()),
    );

    let error = apply(&layout, vec![shared]).await.unwrap_err();

    assert!(
        matches!(&error, DockerError::ConfigurationError(message) if message.contains("under the reclaimable log root") && message.contains("must read {resolved_path}/…/{log_dir_name}")),
        "{error:?}"
    );
}

/// 不带变量、直接写死的宿主机路径也一样：看的是源路径，不是模板长什么样
#[tokio::test]
async fn a_fixed_host_path_into_the_log_root_is_rejected() {
    let layout = Layout::new();
    let fixed = mount(
        "/app/notes",
        layout.host_log_root().join("shared").to_str().unwrap(),
        false,
        None,
    );

    let error = apply(&layout, vec![fixed]).await.unwrap_err();

    assert!(
        matches!(&error, DockerError::ConfigurationError(message) if message.contains("under the reclaimable log root")),
        "{error:?}"
    );
}

/// 最后一级不是整个 `{log_dir_name}` 时，建出来的目录名填回模板会多出一截前缀
#[tokio::test]
async fn a_log_directory_template_with_a_prefix_in_its_last_component_is_rejected() {
    let layout = Layout::new();
    let prefixed = mount(
        "/app/container-logs",
        "{resolved_path}/prefix-{log_dir_name}",
        false,
        Some(layout.visible_log_root().to_str().unwrap()),
    );

    let error = apply(&layout, vec![prefixed]).await.unwrap_err();

    assert!(
        matches!(&error, DockerError::ConfigurationError(message) if message.contains("must read {resolved_path}/…/{log_dir_name}")),
        "{error:?}"
    );
    assert!(
        !layout.host_log_root().exists(),
        "no directory is made for a rejected mount"
    );
}

/// 一个容器可以有几条指向日志根的挂载，每一条都要换成自己的新目录
#[tokio::test]
async fn several_mounts_into_the_log_root_each_get_a_directory_of_their_own() {
    let layout = Layout::new();
    let audit = mount(
        AUDIT_TARGET,
        "{resolved_path}/{log_dir_name}",
        false,
        Some(layout.visible_log_root().to_str().unwrap()),
    );

    let (config, pending) = apply(&layout, vec![audit, log_mount(&layout)])
        .await
        .unwrap();

    assert_eq!(config.extra_mounts.len(), 2);
    let hosts: Vec<&str> = config
        .extra_mounts
        .iter()
        .map(|mount| mount.host_path.as_str())
        .collect();
    assert_ne!(hosts[0], hosts[1], "each mount gets its own directory");
    // 两条挂载在同一秒里偏好同一个名字：后建的撞上了就换名字，各自一个目录
    for mount in &config.extra_mounts {
        let name = file_name(&mount.host_path);
        assert!(name.starts_with(CONTAINER), "{name}");
        assert!(layout.host_log_root().join(name).is_dir());
    }
    let mut claimed = pending_bind_sources();
    claimed.sort();
    let mut expected: Vec<PathBuf> = hosts.iter().map(PathBuf::from).collect();
    expected.sort();
    assert_eq!(claimed, expected, "every fresh directory is claimed");
    // 环境变量只描述日志那一条
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
        Some(file_name(hosts[1]))
    );
    drop(pending);
    assert!(pending_bind_sources().is_empty());
}

/// CONTAINER_LOGS_DIR 写的是实际挂载点（变量已替换），不是配置里的模板。
#[tokio::test]
async fn the_log_directory_env_names_the_path_that_is_actually_mounted() {
    let layout = Layout::new();
    let templated = mount(
        "/app/container-logs/{timestamp}",
        "{resolved_path}/{log_dir_name}",
        false,
        Some(layout.visible_log_root().to_str().unwrap()),
    );

    let config = apply(&layout, vec![templated]).await.unwrap().0;

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

/// 日志根外面的挂载不受影响：目录已经存在就沿用
#[tokio::test]
async fn mounts_outside_the_log_root_keep_using_the_existing_directory() {
    let layout = Layout::new();
    let wallpaper = layout.host_assets_root().join("wallpaper");
    std::fs::create_dir_all(&wallpaper).unwrap();
    std::fs::write(wallpaper.join("keep.txt"), b"kept").unwrap();

    let (config, _pending) = apply(
        &layout,
        vec![mount(
            "/app/wallpaper",
            "{resolved_path}/wallpaper",
            false,
            Some(ASSETS_ROOT),
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
    assert!(pending_bind_sources().is_empty());
    assert!(!config.env_vars.contains_key("CONTAINER_LOG_NAME"));
}

/// 只读挂载在日志根外面是允许的
#[tokio::test]
async fn a_read_only_mount_outside_the_log_root_is_allowed() {
    let layout = Layout::new();

    let (config, _pending) = apply(
        &layout,
        vec![mount(
            "/app/assets",
            "{resolved_path}/static",
            true,
            Some(ASSETS_ROOT),
        )],
    )
    .await
    .unwrap();

    assert!(only_mount(&config).read_only);
    assert!(
        !layout.host_assets_root().join("static").exists(),
        "a read-only mount is not created"
    );
}

/// workspace 这类数据目录落在日志根下面会被清理器当成过期日志回收，拒绝
#[tokio::test]
async fn workspace_data_cannot_live_in_the_log_root() {
    let layout = Layout::new();

    let error = apply_with_workspace(&layout, Vec::new(), Some(&layout.host_log_root()))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, DockerError::ConfigurationError(message) if message.contains("Data mount") && message.contains("reclaimable log root")),
        "{error:?}"
    );
}

/// 容器形态：目录经 RCoder 进程里的路径建出来——它和宿主机路径不是同一个字符串；bind 用
/// 的是宿主机路径。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn in_container_form_the_directory_is_created_through_the_process_path() {
    let layout = Layout::with_separate_views();

    let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap().0;

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

    let config = apply(&layout, vec![log_mount(&layout)]).await.unwrap().0;

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

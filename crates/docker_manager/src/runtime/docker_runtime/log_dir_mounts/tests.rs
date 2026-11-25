//! 判定逻辑的单元测试：真实目录加真实符号链接，不依赖 Docker；
//! 另有用真实 Bollard 客户端对着假 daemon 的协议测试，验证整轮只发一次列表请求。
use super::*;
use crate::runtime::docker_runtime::DockerRuntime;
use container_runtime_api::AgentContainerRuntime as _;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// 假挂载表里宿主机一侧的根，对应 `Tree::base`
const HOST_BASE: &str = "/host/base";
const REAL_A: &str = "/host/base/logs/runner-a";
const ALIAS_A: &str = "/host/base/alias/runner-a";

fn summary(value: Value) -> ContainerSummary {
    serde_json::from_value(value).unwrap()
}

// ─── 清单解析 ───────────────────────────────────────────────────────────────

#[test]
fn mount_sources_collects_every_absolute_source_and_normalizes_it() {
    let containers = [
        summary(json!({"Id": "runner", "Mounts": [
            {"Type": "bind", "Source": "/srv/logs/./runner-a/../runner-a",
             "Destination": "/app/container-logs"},
            {"Type": "volume", "Source": "/var/lib/docker/volumes/data/_data",
             "Destination": "/data"},
            {"Type": "tmpfs", "Destination": "/scratch"}
        ]})),
        summary(json!({"Id": "bare", "Mounts": []})),
    ];

    let sources = mount_sources(&containers).unwrap();

    assert_eq!(
        sources,
        HashSet::from([
            PathBuf::from("/srv/logs/runner-a"),
            PathBuf::from("/var/lib/docker/volumes/data/_data"),
        ])
    );
}

#[test]
fn a_missing_mount_list_is_an_error_rather_than_an_empty_inventory() {
    let error = mount_sources(&[summary(json!({"Id": "opaque", "Mounts": null}))])
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("opaque") && error.contains("no mount list"),
        "{error}"
    );
}

#[test]
fn a_mount_without_a_type_is_an_error() {
    let error = mount_sources(&[summary(json!({"Id": "untyped", "Mounts": [
        {"Source": "/srv/logs/runner-a", "Destination": "/app/container-logs"}
    ]}))])
    .unwrap_err()
    .to_string();

    assert!(
        error.contains("untyped") && error.contains("without a type"),
        "{error}"
    );
}

#[test]
fn a_bind_without_an_absolute_source_is_an_error() {
    for source in [json!("relative/path"), json!(""), Value::Null] {
        let error = mount_sources(&[summary(json!({"Id": "broken", "Mounts": [
            {"Type": "bind", "Source": source, "Destination": "/app/container-logs"}
        ]}))])
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("broken") && error.contains("bind mount without an absolute source"),
            "source {source}: {error}"
        );
    }
}

#[test]
fn mounts_without_a_host_path_are_skipped_for_non_bind_types() {
    let sources = mount_sources(&[summary(json!({"Id": "scratch", "Mounts": [
        {"Type": "tmpfs", "Destination": "/scratch"},
        {"Type": "image", "Source": "sha256:abc", "Destination": "/image"}
    ]}))])
    .unwrap();

    assert!(sources.is_empty());
}

// ─── 路径换算 ───────────────────────────────────────────────────────────────

#[test]
fn reverse_mapping_takes_the_longest_host_prefix_and_refuses_paths_outside() {
    let resolver = HostPathResolver::from_mount_table(vec![
        (PathBuf::from("/app"), PathBuf::from("/host/root")),
        (PathBuf::from("/app/logs"), PathBuf::from("/host/root/logs")),
    ])
    .unwrap();

    assert_eq!(
        resolver.resolve_from_host_path(Path::new("/host/root/logs/container/x")),
        Some(PathBuf::from("/app/logs/container/x"))
    );
    assert_eq!(
        resolver.resolve_from_host_path(Path::new("/host/root/other")),
        Some(PathBuf::from("/app/other"))
    );
    assert_eq!(
        resolver.resolve_from_host_path(Path::new("/elsewhere/logs")),
        None
    );
    assert_eq!(
        resolver
            .resolve_to_host_path(Path::new("/app/logs/./container"))
            .unwrap(),
        PathBuf::from("/host/root/logs/container"),
        "forward and reverse mapping must round-trip"
    );
}

/// 一棵带别名的目录树：`logs/` 是真目录，`alias` 是指向它的相对符号链接，
/// `logs/runner-a` 是被清理的候选。挂载表与生产一致，登记的是上一级目录。
struct Tree {
    _keep: tempfile::TempDir,
    base: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let keep = tempfile::tempdir().unwrap();
        // macOS 的临时目录在 /var 下，而 /var 本身是符号链接：先取物理路径再搭场景
        let base = keep.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("logs")).unwrap();
        std::os::unix::fs::symlink("logs", base.join("alias")).unwrap();
        std::fs::create_dir(base.join("logs").join("runner-a")).unwrap();
        Self { _keep: keep, base }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    fn resolver(&self) -> HostPathResolver {
        HostPathResolver::from_mount_table(vec![(self.base.clone(), PathBuf::from(HOST_BASE))])
            .unwrap()
    }

    /// 以 `sources` 为 Docker 报回的挂载源，走生产同一条路径判定 `candidate`
    async fn verdict(&self, candidate: &Path, sources: &[&str]) -> LogDirMount {
        let resolver = self.resolver();
        let space = HostPathSpace::Container(&resolver);
        Self::decide(&space, candidate, sources).await
    }

    async fn decide(space: &HostPathSpace<'_>, candidate: &Path, sources: &[&str]) -> LogDirMount {
        let sources: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
        let registered = with_ancestors(sources.iter().cloned());
        let physical = physical_mount_points(space, &sources).await;
        classify(candidate, space, &registered, &physical).await
    }
}

#[tokio::test]
async fn an_unmounted_directory_in_a_symlink_free_path_is_reclaimed() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("logs/runner-a"), &[]).await,
        LogDirMount::Unmounted
    );
}

#[tokio::test]
async fn a_directory_registered_on_the_creation_path_is_mounted() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("logs/runner-a"), &[REAL_A]).await,
        LogDirMount::Mounted
    );
}

/// 回归（Codex 第三轮 P1）：创建侧和 Docker 用的都是别名路径。只按解析了符号链接的
/// 物理路径去比，会把一个仍在挂载的目录判成未挂载。
#[tokio::test]
async fn an_alias_registered_mount_keeps_the_candidate_listed_through_the_alias() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("alias/runner-a"), &[ALIAS_A]).await,
        LogDirMount::Mounted
    );
}

#[tokio::test]
async fn a_real_path_registered_mount_keeps_the_candidate_listed_through_the_alias() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("alias/runner-a"), &[REAL_A]).await,
        LogDirMount::Mounted
    );
}

/// 反方向的同一个漏洞：候选走真实路径，容器经别名登记。
#[tokio::test]
async fn an_alias_registered_mount_keeps_the_candidate_listed_through_the_real_path() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("logs/runner-a"), &[ALIAS_A]).await,
        LogDirMount::Mounted
    );
}

#[tokio::test]
async fn a_mount_below_the_candidate_keeps_it_however_it_was_registered() {
    let tree = Tree::new();
    std::fs::create_dir(tree.path("logs/runner-a/nested")).unwrap();

    for source in [
        "/host/base/logs/runner-a/nested",
        "/host/base/alias/runner-a/nested",
    ] {
        assert_eq!(
            tree.verdict(&tree.path("logs/runner-a"), &[source]).await,
            LogDirMount::Mounted,
            "registered through {source}"
        );
    }
}

#[tokio::test]
async fn an_alias_of_another_directory_does_not_keep_an_orphan() {
    let tree = Tree::new();
    std::fs::create_dir(tree.path("logs/runner-b")).unwrap();

    assert_eq!(
        tree.verdict(&tree.path("alias/runner-a"), &["/host/base/alias/runner-b"])
            .await,
        LogDirMount::Unmounted
    );
}

#[tokio::test]
async fn a_candidate_outside_the_mount_table_is_unconfirmed() {
    let tree = Tree::new();

    let verdict = tree
        .verdict(Path::new("/elsewhere/runner-a"), &[REAL_A])
        .await;

    assert!(
        matches!(&verdict, LogDirMount::Unconfirmed(reason) if reason.contains("Map a log directory onto the host")),
        "{verdict:?}"
    );
}

#[tokio::test]
async fn a_candidate_that_vanished_after_listing_is_unmounted() {
    let tree = Tree::new();

    assert_eq!(
        tree.verdict(&tree.path("logs/runner-gone"), &[]).await,
        LogDirMount::Unmounted
    );
}

/// deploy-host 形态不做前缀替换，两边的物理身份都在宿主机文件系统上直接解析，
/// 别名在这里不留死角。
#[tokio::test]
async fn host_space_matches_aliases_by_physical_identity() {
    let tree = Tree::new();
    let real = tree.path("logs/runner-a");
    let alias = tree.path("alias/runner-a");
    let unrelated = tree.path("logs/runner-b");
    let space = HostPathSpace::Host;

    for (source, candidate, expected) in [
        (&real, &real, LogDirMount::Mounted),
        (&alias, &alias, LogDirMount::Mounted),
        (&alias, &real, LogDirMount::Mounted),
        (&real, &alias, LogDirMount::Mounted),
        (&unrelated, &real, LogDirMount::Unmounted),
    ] {
        let source_text = source.to_string_lossy().into_owned();
        assert_eq!(
            Tree::decide(&space, candidate, &[&source_text]).await,
            expected,
            "source {} candidate {}",
            source.display(),
            candidate.display()
        );
    }
}

// ─── 协议：真实 Bollard 客户端 + 假 daemon ───────────────────────────────────

/// 记录每个请求行并返回预设容器列表的假 daemon
struct FakeDaemon {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fake_daemon(list: Value) -> FakeDaemon {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n")
                && stream.read(&mut byte).await.is_ok_and(|read| read == 1)
            {
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            recorded
                .lock()
                .unwrap()
                .push(head.lines().next().unwrap().to_owned());
            let body = list.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _written = stream.write_all(response.as_bytes()).await;
        }
    });
    FakeDaemon {
        address,
        requests,
        server,
    }
}

/// 直接装配 `DockerManager`（不做容器自检），挂载表显式注入
fn runtime(address: std::net::SocketAddr, resolver: HostPathResolver) -> DockerRuntime {
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let manager = Arc::new(DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    DockerRuntime::with_log_path_resolver(manager, resolver)
}

fn bind(id: &str, source: &str) -> Value {
    json!({"Id": id, "Mounts": [
        {"Type": "bind", "Source": source, "Destination": "/app/container-logs"}
    ]})
}

#[tokio::test]
async fn one_list_request_decides_every_candidate() {
    let tree = Tree::new();
    std::fs::create_dir(tree.path("logs/runner-b")).unwrap();
    std::fs::create_dir(tree.path("logs/runner-c")).unwrap();
    let daemon = fake_daemon(json!([
        bind("real-registered", REAL_A),
        bind("alias-registered", "/host/base/alias/runner-c"),
        bind("outside", "/srv/elsewhere/logs")
    ]))
    .await;
    let runtime = runtime(daemon.address, tree.resolver());
    let outside = PathBuf::from("/outside-the-mount-table/runner");
    let candidates = [
        tree.path("logs/runner-a"),
        tree.path("logs/runner-b"),
        tree.path("logs/runner-c"),
        tree.path("alias/runner-a"),
        outside.clone(),
    ];

    let mounts = runtime.mounted_log_directories(&candidates).await.unwrap();

    assert_eq!(
        mounts.mounted,
        HashSet::from([
            tree.path("logs/runner-a"),
            tree.path("logs/runner-c"),
            tree.path("alias/runner-a"),
        ])
    );
    assert!(
        matches!(
            mounts.unconfirmed.as_slice(),
            [(path, reason)] if path == &outside && reason.contains("Map a log directory onto the host")
        ),
        "{:?}",
        mounts.unconfirmed
    );

    let requests = daemon.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        1,
        "five candidates must cost exactly one request: {requests:?}"
    );
    assert!(
        requests[0].starts_with("GET ") && requests[0].contains("/containers/json"),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn a_malformed_inventory_fails_the_whole_lookup() {
    let tree = Tree::new();
    let daemon = fake_daemon(json!([{"Id": "opaque", "Mounts": null}])).await;
    let runtime = runtime(daemon.address, tree.resolver());

    let error = runtime
        .mounted_log_directories(&[tree.path("logs/runner-a")])
        .await
        .unwrap_err();

    let error = error.to_string();
    assert!(
        error.contains("opaque") && error.contains("no mount list"),
        "{error}"
    );
}

//! 真实 Docker 见证：真实的 `DockerRuntime` 加真实的 `LogCleaner`，不使用替身。
//!
//! 显式开启才运行。需要三个环境变量：
//!
//! - `RCODER_LOG_CLEANER_DOCKER_IMAGE`：本地 Alpine 镜像 ID。不拉取镜像。
//! - `RCODER_LOG_CLEANER_TEST_ROOT`：Docker 能 bind 的可写目录（如 `/tmp`）。测试在里面建两个
//!   目录，分别充当 RCoder 进程一侧与宿主机一侧的日志根，两边前缀不同，与 compose 一致。
//! - `HOSTNAME`：任一连接了网络的真实容器的 ID。`DockerManager::new` 检测主网络时按它
//!   inspect；日志路径的挂载表由测试显式注入，与它无关。
//!
//! 只移除本次运行创建的见证容器（按容器 ID，不带 `--volumes`）。

use super::cleaner::LogCleaner;
use super::owner::RuntimeMountedPaths;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const OWNER_LABEL: &str = "rcoder.validation.log-cleaner";
fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("run the docker CLI")
}

fn image() -> String {
    let image =
        std::env::var("RCODER_LOG_CLEANER_DOCKER_IMAGE").expect("explicit local fixture image");
    assert!(
        image.starts_with("sha256:") && image.len() == 71,
        "use an exact existing local image ID"
    );
    image
}

/// 与生产布局一致的目录名：`<容器名>-<时间戳>`
fn dir_name(prefix: &str, owner: &str) -> String {
    format!("rcoder-log-cleaner-{prefix}-{owner}-20261029035324")
}

/// 本次运行创建的见证容器；离开作用域时移除
struct Witness {
    id: String,
}

impl Drop for Witness {
    fn drop(&mut self) {
        let _removed = docker(&["rm", "--force", &self.id]);
    }
}

/// 建一个内容为空的目录并把它标记为 `days` 天前修改
fn aged_dir(parent: &Path, name: &str, days: u64) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("create dir");
    let old_time = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time))
        .expect("set directory mtime");
    dir
}

/// 起一个见证容器；`mount` 有值时把该宿主机目录挂到 `/app/container-logs`，
/// 否则不带任何挂载
fn run_witness(image: &str, owner: &str, name: &str, mount: Option<&Path>) -> Witness {
    let label = format!("{OWNER_LABEL}={owner}");
    let bind = mount.map(|host_dir| {
        format!(
            "type=bind,src={},dst=/app/container-logs",
            host_dir.display()
        )
    });
    let mut args = vec![
        "run",
        "--pull=never",
        "--detach",
        "--network",
        "none",
        "--name",
        name,
        "--label",
        &label,
    ];
    if let Some(bind) = &bind {
        args.extend(["--mount", bind.as_str()]);
    }
    args.extend([image, "sleep", "600"]);
    let created = docker(&args);
    assert!(
        created.status.success(),
        "owned fixture creation failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    // 先绑定清理，再做后续断言：断言失败也不能留下容器
    let witness = Witness { id: id.clone() };
    assert!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unexpected container id: {id}"
    );
    witness
}

/// 一次见证的脚手架。进程侧根 `process` 经注入的挂载表映射到宿主机侧根 `host`：生产里
/// 前者是 rcoder 容器内的路径，后者是 Docker daemon 看到的路径。
struct Sandbox {
    _process: TempDir,
    _host: TempDir,
    process: PathBuf,
    host: PathBuf,
    resolver: docker_manager::HostPathResolver,
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
}

impl Sandbox {
    async fn new() -> Self {
        let scratch = std::fs::canonicalize(
            std::env::var_os("RCODER_LOG_CLEANER_TEST_ROOT")
                .expect("a scratch root that Docker can bind"),
        )
        .unwrap();
        let process = TempDir::new_in(&scratch).unwrap();
        let host = TempDir::new_in(&scratch).unwrap();
        let resolver = docker_manager::HostPathResolver::from_mount_table_for_tests(vec![(
            process.path().to_path_buf(),
            host.path().to_path_buf(),
        )])
        .unwrap();
        let manager = Arc::new(
            docker_manager::DockerManager::new(docker_manager::DockerManagerConfig::default())
                .await
                .expect("connect to the local Docker daemon"),
        );
        // 直接落成 trait object，和 `state.runtime()` 的形态一致。
        let runtime: Arc<dyn container_runtime_api::ContainerRuntime> = Arc::new(
            docker_manager::runtime::DockerRuntime::with_log_path_resolver_for_tests(
                manager,
                resolver.clone(),
            ),
        );
        Self {
            process: process.path().to_path_buf(),
            host: host.path().to_path_buf(),
            _process: process,
            _host: host,
            resolver,
            runtime,
        }
    }

    /// `local`（进程侧）对应的宿主机侧路径，父目录在宿主机侧建好
    fn host_path(&self, local: &Path) -> PathBuf {
        let path = self.resolver.resolve_to_host_path(local).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleaner(&self, log_dir: &Path) -> LogCleaner {
        LogCleaner::new(
            log_dir.to_str().unwrap(),
            10,
            Arc::new(RuntimeMountedPaths::new(Arc::clone(&self.runtime))),
        )
    }
}

#[tokio::test]
#[ignore = "requires Docker, RCODER_LOG_CLEANER_DOCKER_IMAGE (a local Alpine image ID; never pulls images), RCODER_LOG_CLEANER_TEST_ROOT and HOSTNAME; see the module docs"]
async fn physical_docker_keeps_every_mounted_dir_and_reclaims_the_rest() {
    let image = image();
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = Sandbox::new().await;
    // 一个没有任何挂载的容器：真实 Docker 对它报回空的挂载列表，清单查询必须照常成功
    // 绑定一直持有到函数结束，离开时移除容器
    let _bare = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-bare-{owner}"),
        None,
    );

    // 四个目录按修改时间都已过期；区别只在于当前有没有容器挂着
    let mounted_running = aged_dir(&sandbox.process, &dir_name("running", &owner), 15);
    let mounted_stopped = aged_dir(&sandbox.process, &dir_name("stopped", &owner), 15);
    // 目录名用的是容器创建时的名字；容器随后被改名，这个名字已经查不到任何容器
    let mounted_renamed = aged_dir(&sandbox.process, &dir_name("renamed", &owner), 15);
    let unmounted = aged_dir(&sandbox.process, &dir_name("gone", &owner), 15);

    let child = mounted_running.join("nested");
    std::fs::create_dir(&child).unwrap();
    // Restore the parent mtime after preparing the nested bind source.
    std::fs::set_times(
        &mounted_running,
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(15 * 86400)),
    )
    .unwrap();
    let host_running = sandbox.host_path(&child);
    let host_stopped = sandbox.host_path(&mounted_stopped);
    let host_renamed = sandbox.host_path(&mounted_renamed);
    let running = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-running-{owner}"),
        Some(&host_running),
    );
    let stopped = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-stopped-{owner}"),
        Some(&host_stopped),
    );
    let renamed = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-renamed-{owner}"),
        Some(&host_renamed),
    );
    // 停止但保留容器：Docker 仍然报告它的挂载
    let stopped_result = docker(&["stop", "--time", "2", &stopped.id]);
    assert!(stopped_result.status.success(), "stop fixture failed");
    // 改名：原名 inspect 会 404，但挂载还在
    let new_name = format!("rcoder-log-cleaner-renamed-away-{owner}");
    let rename_result = docker(&["rename", &renamed.id, &new_name]);
    assert!(rename_result.status.success(), "rename fixture failed");
    let by_old_name = docker(&[
        "inspect",
        &format!("rcoder-log-cleaner-renamed-{owner}"),
        "--format",
        "{{.Id}}",
    ]);
    assert!(
        !by_old_name.status.success(),
        "precondition: the original name must no longer resolve"
    );

    // 运行时自检：清单里必须包含三个被挂载的目录，且不包含没挂的那个
    let inventory = sandbox
        .runtime
        .mounted_host_paths()
        .await
        .expect("a container without mounts must not break the inventory");
    for mounted in [&host_running, &host_stopped, &host_renamed] {
        assert!(
            inventory.contains(mounted),
            "inventory must include the mounted path {}",
            mounted.display()
        );
    }
    assert!(
        !inventory.contains(&sandbox.host_path(&unmounted)),
        "inventory must not include the unmounted path"
    );

    let cleaner = sandbox.cleaner(&sandbox.process);
    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(
        mounted_running.exists(),
        "directory of a running container must be kept"
    );
    assert!(
        mounted_stopped.exists(),
        "directory of a stopped-but-present container must be kept"
    );
    assert!(
        mounted_renamed.exists(),
        "directory of a renamed container must be kept: its name no longer resolves, \
         but the mount is still there"
    );
    assert!(
        !unmounted.exists(),
        "directory no container mounts must be reclaimed"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 3);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);

    // 容器回收后挂载消失，下一轮同一目录就能删掉
    drop(renamed);
    drop(stopped);
    drop(running);
    let inventory = sandbox
        .runtime
        .mounted_host_paths()
        .await
        .expect("inventory");
    assert!(
        !inventory.contains(&host_renamed),
        "precondition: mount must be gone after the container is removed"
    );
    let second = cleaner.cleanup_once().await.unwrap();
    assert!(!mounted_running.exists());
    assert!(!mounted_stopped.exists());
    assert!(!mounted_renamed.exists());
    assert_eq!(second.dirs_deleted, 3);
}

/// 回归（Codex 第三轮 P1）：日志根本身是一层符号链接。创建侧和 Docker 用的都是别名路径，
/// 按解析了符号链接的物理路径去比，会把仍在挂载的目录判成未挂载而删掉。
///
/// 覆盖三种登记方式：容器经别名登记、容器经真实路径登记、没有容器挂。前两个必须保留，
/// 第三个必须回收；容器移除后前两个也能回收。
#[tokio::test]
#[ignore = "requires Docker, RCODER_LOG_CLEANER_DOCKER_IMAGE (a local Alpine image ID; never pulls images), RCODER_LOG_CLEANER_TEST_ROOT and HOSTNAME; see the module docs"]
async fn physical_docker_keeps_dirs_mounted_through_a_symlinked_log_root() {
    let image = image();
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = Sandbox::new().await;

    // 进程侧：`container-real` 是真目录，`container` 是指向它的相对符号链接。宿主机侧做成
    // 同样的结构：Docker 解析宿主机上的符号链接后挂的是真目录，Source 报的却是别名。
    for root in [&sandbox.process, &sandbox.host] {
        std::fs::create_dir(root.join("container-real")).unwrap();
        std::os::unix::fs::symlink("container-real", root.join("container")).unwrap();
    }
    let log_dir = sandbox.process.join("container");
    let by_alias = aged_dir(&log_dir, &dir_name("alias", &owner), 15);
    let by_real = aged_dir(&log_dir, &dir_name("real", &owner), 15);
    let orphan = aged_dir(&log_dir, &dir_name("orphan", &owner), 15);
    let host = &sandbox.host;
    let alias_registered = host.join("container").join(by_alias.file_name().unwrap());
    let real_registered = host
        .join("container-real")
        .join(by_real.file_name().unwrap());
    // 两侧的目录树各自独立：宿主机一侧的目录要单独建出来，Docker 才有东西可挂
    for mounted in [&alias_registered, &real_registered] {
        std::fs::create_dir_all(mounted).unwrap();
    }
    let alias = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-alias-{owner}"),
        Some(&alias_registered),
    );
    let real = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-real-{owner}"),
        Some(&real_registered),
    );
    // 前置：Docker 报回的确实是登记时的路径，一个别名、一个真实路径
    let inventory = sandbox
        .runtime
        .mounted_host_paths()
        .await
        .expect("inventory");
    assert!(
        inventory.contains(&alias_registered) && inventory.contains(&real_registered),
        "Docker must report both registered paths as given: {inventory:?}"
    );

    let cleaner = sandbox.cleaner(&log_dir);
    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(
        by_alias.exists(),
        "a directory mounted through the alias must be kept"
    );
    assert!(
        by_real.exists(),
        "a directory mounted through the real path must be kept"
    );
    assert!(
        !orphan.exists(),
        "an unmounted directory under the symlinked root must be reclaimed"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 2);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);

    drop(real);
    drop(alias);
    let second = cleaner.cleanup_once().await.unwrap();
    assert!(!by_alias.exists() && !by_real.exists());
    assert_eq!(second.dirs_deleted, 2);
}

/// 回归（Codex 第五轮 P1）：挂载源的路径中间有一个一直稳定的符号链接，指向候选里面的深层
/// 子目录。Docker 报回的是别名路径，挂载的是它指向的目录；候选是那个目录的真实父目录，
/// 按别名路径的词法父级找不到它。
///
/// 别名放在路径中间而不是最后一级：最后一级是链接时 Docker 并不会把目标目录挂进容器，
/// 那种容器本身就拿不到挂载。
#[tokio::test]
#[ignore = "requires Docker, RCODER_LOG_CLEANER_DOCKER_IMAGE (a local Alpine image ID; never pulls images), RCODER_LOG_CLEANER_TEST_ROOT and HOSTNAME; see the module docs"]
async fn physical_docker_keeps_the_real_ancestors_of_a_dir_mounted_through_a_stable_alias() {
    let image = image();
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = Sandbox::new().await;

    // 两侧各自建出同样的结构：<候选>/log/sub 是真目录，日志根下的 latest 指向 <候选>/log
    let name = dir_name("aliased", &owner);
    let mut candidate = None;
    for root in [&sandbox.process, &sandbox.host] {
        let nested = root.join(&name).join("log/sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inner.log"), "from-sub").unwrap();
        std::os::unix::fs::symlink(Path::new(&name).join("log"), root.join("latest")).unwrap();
        if candidate.is_none() {
            candidate = Some(root.join(&name));
        }
    }
    let candidate = candidate.unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 86400);
    std::fs::set_times(&candidate, std::fs::FileTimes::new().set_modified(old_time)).unwrap();
    let orphan = aged_dir(&sandbox.process, &dir_name("orphan", &owner), 15);
    // 容器经别名登记：路径中间经过 latest
    let registered = sandbox.host.join("latest").join("sub");
    let aliased = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-aliased-{owner}"),
        Some(&registered),
    );

    // 前置：Docker 报回的是别名路径，挂进去的却是别名指向的目录
    let inventory = sandbox
        .runtime
        .mounted_host_paths()
        .await
        .expect("inventory");
    assert!(
        inventory.contains(&registered),
        "Docker must report the registered path as given: {inventory:?}"
    );
    let inside = docker(&["exec", &aliased.id, "cat", "/app/container-logs/inner.log"]);
    assert!(
        inside.status.success() && String::from_utf8_lossy(&inside.stdout).trim() == "from-sub",
        "the container must be serving the directory the alias points to: {}",
        String::from_utf8_lossy(&inside.stderr)
    );

    let cleaner = sandbox.cleaner(&sandbox.process);
    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(
        candidate.exists(),
        "a directory that contains the real location of a mounted path must be kept"
    );
    assert!(
        !orphan.exists(),
        "a directory no container mounts must be reclaimed"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 1);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);

    drop(aliased);
    let second = cleaner.cleanup_once().await.unwrap();
    assert!(!candidate.exists());
    assert_eq!(second.dirs_deleted, 1);
}

/// 回归（Codex 第六轮 P1）：挂载源的路径里有一个 RCoder **看得到**的链接，它指向 RCoder 没有挂载的
/// 宿主机路径，那里的链接又指回候选里面的目录。容器经 `<根>/latest/sub` 登记，实际挂的是
/// `<候选>/log/sub`。RCoder 读到 `latest` 之后走进看不见的路径，那里还有没有链接绕回来它无从知道，
/// 不能当成"与候选无关"。
///
/// 见证里用第三个目录充当 RCoder 没有挂载的宿主机路径，与进程侧、宿主机侧并列。
#[tokio::test]
#[ignore = "requires Docker, RCODER_LOG_CLEANER_DOCKER_IMAGE (a local Alpine image ID; never pulls images), RCODER_LOG_CLEANER_TEST_ROOT and HOSTNAME; see the module docs"]
async fn physical_docker_keeps_a_candidate_that_a_visible_link_reaches_through_an_unseen_path() {
    let image = image();
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let sandbox = Sandbox::new().await;
    let scratch = std::fs::canonicalize(
        std::env::var_os("RCODER_LOG_CLEANER_TEST_ROOT")
            .expect("a scratch root that Docker can bind"),
    )
    .unwrap();
    let unseen = TempDir::new_in(&scratch).unwrap();
    let unseen = unseen.path().canonicalize().unwrap();

    // 两侧各自建出同样的结构：<候选>/log/sub/inner.log，日志根下的 latest 指向看不见的位置
    let name = dir_name("reentered", &owner);
    let mut candidate = None;
    for root in [&sandbox.process, &sandbox.host] {
        let nested = root.join(&name).join("log/sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inner.log"), "from-sub").unwrap();
        std::os::unix::fs::symlink(unseen.join("reentry"), root.join("latest")).unwrap();
        if candidate.is_none() {
            candidate = Some(root.join(&name));
        }
    }
    let candidate = candidate.unwrap();
    // 看不见的位置上有一个链接，指回宿主机一侧的 <候选>/log
    std::os::unix::fs::symlink(sandbox.host.join(&name).join("log"), unseen.join("reentry"))
        .unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 86400);
    std::fs::set_times(&candidate, std::fs::FileTimes::new().set_modified(old_time)).unwrap();
    let orphan = aged_dir(&sandbox.process, &dir_name("orphan", &owner), 15);
    let registered = sandbox.host.join("latest").join("sub");
    let reentered = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-reentered-{owner}"),
        Some(&registered),
    );

    // 前置：Docker 报回的是登记时的路径；挂进去的是经两个链接绕回候选里面的那个目录
    let inventory = sandbox
        .runtime
        .mounted_host_paths()
        .await
        .expect("inventory");
    assert!(
        inventory.contains(&registered),
        "Docker must report the registered path as given: {inventory:?}"
    );
    let inside = docker(&[
        "exec",
        &reentered.id,
        "cat",
        "/app/container-logs/inner.log",
    ]);
    assert!(
        inside.status.success() && String::from_utf8_lossy(&inside.stdout).trim() == "from-sub",
        "the container must be serving the directory inside the candidate: {}",
        String::from_utf8_lossy(&inside.stderr)
    );

    let cleaner = sandbox.cleaner(&sandbox.process);
    let stats = cleaner.cleanup_once().await.unwrap();

    // 无法确认的源可能指向任何候选，所以是整轮保留：无关的孤儿这一轮也留着
    assert!(
        candidate.exists(),
        "a candidate the mount may reach through an unseen path must be kept"
    );
    assert!(
        orphan.exists(),
        "one unconfirmable mount source keeps every candidate for the cycle"
    );
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_skipped_unconfirmed, 2);
    assert_eq!(stats.dirs_skipped_in_use, 0);

    drop(reentered);
    let second = cleaner.cleanup_once().await.unwrap();
    assert!(!candidate.exists() && !orphan.exists());
    assert_eq!(second.dirs_deleted, 2);
}

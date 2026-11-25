//! 真实 Docker 见证：真实的 `DockerRuntime` 加真实的 `LogCleaner`，不使用替身。
//!
//! 显式开启才运行。需要一个本地的 Alpine 镜像 ID，不拉取镜像；只移除本次运行创建的
//! 见证容器（按容器 ID，不带 `--volumes`）。
//!
//! 覆盖四种真实形态，其中"改名后仍保留"是审查意见 P1-1 的回归：按容器名已经查不到它，
//! 但挂载还在，目录必须活。

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

/// 本次运行创建的见证容器；离开作用域时移除
struct Witness {
    id: String,
}

impl Drop for Witness {
    fn drop(&mut self) {
        drop(docker(&["rm", "--force", &self.id]));
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

/// 起一个把 `host_dir` 挂到 `/app/container-logs` 的容器
fn run_witness(image: &str, owner: &str, name: &str, host_dir: &Path) -> Witness {
    let created = docker(&[
        "run",
        "--pull=never",
        "--detach",
        "--network",
        "none",
        "--name",
        name,
        "--label",
        &format!("{OWNER_LABEL}={owner}"),
        "--mount",
        &format!(
            "type=bind,src={},dst=/app/container-logs",
            host_dir.display()
        ),
        image,
        "sleep",
        "600",
    ]);
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

#[tokio::test]
#[ignore = "requires Docker and RCODER_LOG_CLEANER_DOCKER_IMAGE set to a local Alpine image ID; never pulls images"]
async fn physical_docker_keeps_every_mounted_dir_and_reclaims_the_rest() {
    let image =
        std::env::var("RCODER_LOG_CLEANER_DOCKER_IMAGE").expect("explicit local fixture image");
    assert!(
        image.starts_with("sha256:") && image.len() == 71,
        "use an exact existing local image ID"
    );
    let owner = uuid::Uuid::new_v4().simple().to_string();

    // HOSTNAME names a witness that maps the test process's local root to a
    // Docker host root. The two path prefixes may differ, as in Compose.
    let root = std::env::var_os("RCODER_LOG_CLEANER_TEST_ROOT").expect("test mount root");
    let log_root = TempDir::new_in(std::fs::canonicalize(root).unwrap()).unwrap();
    let resolver = docker_manager::HostPathResolver::new().await.unwrap();
    let host_root = resolver.resolve_to_host_path(log_root.path()).unwrap();
    std::fs::create_dir_all(&host_root).unwrap();
    // 四个目录按修改时间都已过期；区别只在于当前有没有容器挂着
    let mounted_running = aged_dir(
        log_root.path(),
        &format!("rcoder-log-cleaner-running-{owner}-20261029035324"),
        15,
    );
    let mounted_stopped = aged_dir(
        log_root.path(),
        &format!("rcoder-log-cleaner-stopped-{owner}-20261029035324"),
        15,
    );
    // 目录名用的是容器创建时的名字；容器随后被改名，这个名字已经查不到任何容器
    let mounted_renamed = aged_dir(
        log_root.path(),
        &format!("rcoder-log-cleaner-renamed-{owner}-20261029035324"),
        15,
    );
    let unmounted = aged_dir(
        log_root.path(),
        &format!("rcoder-log-cleaner-gone-{owner}-20261029035324"),
        15,
    );

    let child = mounted_running.join("nested");
    std::fs::create_dir(&child).unwrap();
    // Restore the parent mtime after preparing the nested bind source.
    std::fs::set_times(
        &mounted_running,
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(15 * 86400)),
    )
    .unwrap();
    let host_source = |local: &Path| {
        let path = resolver.resolve_to_host_path(local).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        path
    };
    let host_running = host_source(&child);
    let host_stopped = host_source(&mounted_stopped);
    let host_renamed = host_source(&mounted_renamed);
    let running = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-running-{owner}"),
        &host_running,
    );
    let stopped = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-stopped-{owner}"),
        &host_stopped,
    );
    let renamed = run_witness(
        &image,
        &owner,
        &format!("rcoder-log-cleaner-renamed-{owner}"),
        &host_renamed,
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

    // 与生产相同的构造方式：真实 DockerManager → 真实 DockerRuntime
    let manager = Arc::new(
        docker_manager::DockerManager::new(docker_manager::DockerManagerConfig::default())
            .await
            .expect("connect to the local Docker daemon"),
    );
    // 直接落成 trait object，和 `state.runtime()` 的形态一致。
    let runtime: Arc<dyn container_runtime_api::ContainerRuntime> =
        Arc::new(docker_manager::runtime::DockerRuntime::new(manager));

    // 运行时自检：清单里必须包含三个被挂载的目录，且不包含没挂的那个
    let inventory = runtime.mounted_host_paths().await.expect("inventory");
    for mounted in [&host_running, &host_stopped, &host_renamed] {
        assert!(
            inventory.contains(mounted),
            "inventory must include the mounted path {}",
            mounted.display()
        );
    }
    assert!(
        !inventory.contains(&resolver.resolve_to_host_path(&unmounted).unwrap()),
        "inventory must not include the unmounted path"
    );

    let cleaner = LogCleaner::new(
        log_root.path().to_str().unwrap(),
        10,
        Arc::new(RuntimeMountedPaths::new(Arc::clone(&runtime))),
    );

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
    let inventory = runtime.mounted_host_paths().await.expect("inventory");
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

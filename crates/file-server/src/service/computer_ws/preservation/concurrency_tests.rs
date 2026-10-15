use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

use crate::service::computer_ws::create_workspace;

use super::{DYNAMIC_ADD_LOCK, lock, test_gate};

const ROOT_ENV: &str = "RCODER_PRESERVATION_CHILD_ROOT";
const MODE_ENV: &str = "RCODER_PRESERVATION_CHILD_MODE";
const CHILD_TEST: &str =
    "service::computer_ws::preservation::concurrency_tests::physical_lock_child_process_fixture";

fn seed(root: &Path) {
    let skill = root.join(".agents/skills/skill-a");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join(DYNAMIC_ADD_LOCK), b"").unwrap();
    std::fs::write(skill.join("SKILL.md"), b"original-locked-skill").unwrap();
}

fn assert_skill(root: &Path) {
    assert_eq!(
        std::fs::read(root.join(".agents/skills/skill-a/SKILL.md")).unwrap(),
        b"original-locked-skill"
    );
    assert!(!root.join(".agents/.preserved-skills/receipt.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn receipt_publish_and_cleanup_never_follow_a_replaced_parent() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let area = super::preserve_area(&root);
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(&area).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let path = area.join("receipt.json");
    std::fs::write(&path, b"previous-complete").unwrap();
    std::fs::write(outside.join("receipt.json"), b"foreign-sentinel").unwrap();
    let captured = super::receipt::Captured::capture(&root, &path)
        .unwrap()
        .unwrap();
    let gate = test_gate::register_receipt_check(&root);
    let operation = tokio::task::spawn_blocking(move || captured.write_bytes(b"next-complete"));
    gate.entered().await;
    let detached = root.join(".agents/detached-preservation");
    std::fs::rename(&area, &detached).unwrap();
    std::os::unix::fs::symlink(&outside, &area).unwrap();
    gate.release();
    assert!(
        operation.await.unwrap().is_err(),
        "changed binding must be explicit"
    );
    assert_eq!(
        std::fs::read(outside.join("receipt.json")).unwrap(),
        b"foreign-sentinel"
    );
    assert_eq!(
        std::fs::read(detached.join("receipt.json")).unwrap(),
        b"next-complete"
    );
    let detached_receipt = super::receipt::Captured::capture(&root, &detached.join("receipt.json"))
        .unwrap()
        .unwrap();
    std::fs::rename(&detached, root.join(".agents/second-detached")).unwrap();
    std::os::unix::fs::symlink(&outside, &detached).unwrap();
    assert!(detached_receipt.remove().is_err());
    assert_eq!(
        std::fs::read(outside.join("receipt.json")).unwrap(),
        b"foreign-sentinel"
    );
    assert_eq!(
        std::fs::read(root.join(".agents/second-detached/receipt.json")).unwrap(),
        b"next-complete"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_receipt_is_rejected_without_consuming_workspace_workers() {
    use std::os::unix::fs::FileTypeExt as _;
    let directory = tempfile::tempdir().unwrap();
    for index in 0..5 {
        let root = directory.path().join(format!("workspace-{index}"));
        seed(&root);
        std::fs::create_dir_all(super::preserve_area(&root)).unwrap();
        let fifo = super::preserve_receipt_path(&root);
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            create_workspace(&root, None, Vec::new(), None, None),
        )
        .await
        .expect("FIFO must not leave an actual worker blocked");
        assert!(result.is_err());
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
    let healthy = directory.path().join("healthy");
    seed(&healthy);
    tokio::time::timeout(
        Duration::from_secs(10),
        create_workspace(&healthy, None, Vec::new(), None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_skill(&healthy);
}

#[cfg(unix)]
#[tokio::test]
async fn captured_skill_parents_do_not_follow_a_replaced_foreign_ancestor() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(root.join("source/deep/skill-a")).unwrap();
    std::fs::create_dir_all(root.join("target/deep")).unwrap();
    std::fs::create_dir_all(outside.join("deep/skill-a")).unwrap();
    std::fs::write(root.join("source/deep/skill-a/SKILL.md"), b"original").unwrap();
    std::fs::write(outside.join("deep/skill-a/SKILL.md"), b"foreign").unwrap();
    let source = root.join("source/deep/skill-a");
    let target = root.join("target/deep/skill-a");
    let expected = lock::identity(&source, true).unwrap();
    let gate = test_gate::register_rename_capture(&root);
    let scope = root.clone();
    let operation = tokio::spawn(async move {
        super::move_exact_directory(&scope, &source, &target, &expected).await
    });
    gate.entered().await;
    std::fs::rename(root.join("source"), root.join("detached-source")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("source")).unwrap();
    std::fs::rename(root.join("target"), root.join("detached-target")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("target")).unwrap();
    gate.release();
    let result = tokio::time::timeout(Duration::from_secs(20), operation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "replaced visible bindings must be reported explicitly"
    );
    assert_eq!(
        std::fs::read(outside.join("deep/skill-a/SKILL.md")).unwrap(),
        b"foreign",
        "foreign directory must never be moved or replaced"
    );
    assert_eq!(
        std::fs::read(root.join("detached-target/deep/skill-a/SKILL.md")).unwrap(),
        b"original"
    );
    assert!(!root.join("detached-source/deep/skill-a").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_observer_and_workspace_alias_keep_actual_worker_locked() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let alias = directory.path().join("alias");
    seed(&root);
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    let gate = test_gate::register(&root);
    let first_root = root.clone();
    let first =
        tokio::spawn(
            async move { create_workspace(&first_root, None, Vec::new(), None, None).await },
        );
    gate.entered().await;
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    let second =
        tokio::spawn(async move { create_workspace(&alias, None, Vec::new(), None, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !second.is_finished(),
        "alias must wait for the actual first worker"
    );
    let lock_file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(root.join(".agents/.skill-preservation.lock"))
        .unwrap();
    assert!(matches!(
        lock_file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    gate.release();
    tokio::time::timeout(Duration::from_secs(20), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_skill(&root);
}

#[tokio::test]
async fn cancelled_observer_retains_physical_lock_until_worker_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    seed(&root);
    let gate = test_gate::register(&root);
    let first_root = root.clone();
    let first =
        tokio::spawn(
            async move { create_workspace(&first_root, None, Vec::new(), None, None).await },
        );
    gate.entered().await;
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    let first_lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(root.join(".agents/.skill-preservation.lock"))
        .unwrap();
    assert!(matches!(
        first_lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let second_root = root.clone();
    let second =
        tokio::spawn(
            async move { create_workspace(&second_root, None, Vec::new(), None, None).await },
        );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!second.is_finished());
    gate.release();
    tokio::time::timeout(Duration::from_secs(20), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_skill(&root);
}

struct ChildFixture(Child);

impl Drop for ChildFixture {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

fn child(root: &Path, mode: &str) -> ChildFixture {
    ChildFixture(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env(ROOT_ENV, root)
            .env(MODE_ENV, mode)
            .spawn()
            .unwrap(),
    )
}

async fn wait_marker(path: &Path) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !tokio::fs::try_exists(path).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child marker");
}

async fn wait_child(child: &mut ChildFixture) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "child fixture failed: {status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child fixture exited within its external deadline");
}

#[tokio::test]
async fn public_workspace_creation_waits_for_another_process_directory_lock() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    seed(&root);
    let mut child = child(&root, "hold");
    wait_marker(&root.join("child-ready")).await;
    let work_root = root.clone();
    let request =
        tokio::spawn(
            async move { create_workspace(&work_root, None, Vec::new(), None, None).await },
        );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !request.is_finished(),
        "cross-process lock must protect the full create chain"
    );
    assert_eq!(
        std::fs::read(root.join(".agents/skills/skill-a/SKILL.md")).unwrap(),
        b"original-locked-skill"
    );
    std::fs::write(root.join("child-release"), b"").unwrap();
    wait_child(&mut child).await;
    tokio::time::timeout(Duration::from_secs(20), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_skill(&root);
}

#[tokio::test]
async fn public_workspace_creation_completes_with_one_tokio_blocking_slot() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    seed(&root);
    let mut child = child(&root, "single-slot");
    wait_child(&mut child).await;
    assert_skill(&root);
    assert!(root.join("child-completed").is_file());
}

#[test]
fn physical_lock_child_process_fixture() {
    let Some(root) = std::env::var_os(ROOT_ENV) else {
        // The fixture is also an actual native lock contract test on its own.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        let first = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let second = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var(MODE_ENV).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    match mode.as_str() {
        "hold" => {
            let _held = runtime.block_on(lock::acquire(&root)).unwrap();
            std::fs::write(root.join("child-ready"), b"").unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !root.join("child-release").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent did not release child"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "single-slot" => {
            runtime
                .block_on(create_workspace(&root, None, Vec::new(), None, None))
                .unwrap();
            assert_skill(&root);
            std::fs::write(root.join("child-completed"), b"").unwrap();
        }
        other => panic!("unsupported child fixture mode {other}"),
    }
}

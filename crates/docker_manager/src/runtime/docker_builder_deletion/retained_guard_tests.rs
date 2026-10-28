//! DRAFT: real local FDs/flock and independent processes; no Docker or cluster.
//! Register this module from docker_builder_deletion/mod.rs before compiling it.
#![cfg(unix)]
use super::*;
use shared_types::{AppOperationLease as _, UserAppOperationLeaseReceipt};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

const OWNER: &str = "originalexecutor";
const CHILD_TEST: &str = "runtime::docker_builder_deletion::retained_guard_tests::two_independent_processes_cannot_both_resume_the_original_guard";

fn retained(root: &Path) -> (std::path::PathBuf, UserAppOperationLeaseReceipt) {
    let guard = lock_builder_file_with_marker(
        root,
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation(OWNER).unwrap(),
    )
    .unwrap();
    let receipt = guard.receipt().unwrap();
    let path = root.join("builder-app.lock");
    drop(guard); // A process exit/drop releases the FD lock, never the marker.
    assert_eq!(std::fs::read(&path).unwrap(), OWNER.as_bytes());
    (path, receipt)
}

#[tokio::test]
async fn retained_original_guard_uses_its_locked_fd_and_drop_preserves_the_marker() {
    let root = tempfile::tempdir().unwrap();
    let (path, receipt) = retained(root.path());
    let guard = resume_original_builder_file(&path, &receipt, OWNER).unwrap();
    assert_eq!(guard.receipt(), Some(receipt.clone()));
    let UserAppOperationLeaseReceipt::Docker {
        device,
        inode,
        token,
        ..
    } = &receipt
    else {
        panic!("Docker receipt required");
    };
    let metadata = guard.file.metadata().unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), (*device, *inode));
    assert_eq!(token, OWNER);
    assert!(
        guard.validate_retained().await.unwrap(),
        "validation must inspect the retained FD, not a second conflicting open"
    );

    let contender = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(resume_original_builder_file(&path, &receipt, OWNER).is_err());
    drop(guard);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        OWNER.as_bytes(),
        "Drop cannot acknowledge completion"
    );
    contender.try_lock().unwrap();
    contender.unlock().unwrap();
    let recovered = resume_original_builder_file(&path, &receipt, OWNER).unwrap();
    assert!(recovered.validate_retained().await.unwrap());
    Box::new(recovered).release().await.unwrap();
    assert!(
        std::fs::read(&path).unwrap().is_empty(),
        "only explicit release clears the marker"
    );
    assert!(resume_original_builder_file(&path, &receipt, OWNER).is_err());
}

#[tokio::test]
async fn retained_guard_would_block_never_becomes_success() {
    let root = tempfile::tempdir().unwrap();
    let (path, receipt) = retained(root.path());
    let guard = resume_original_builder_file(&path, &receipt, OWNER).unwrap();
    // Deliberately invalidate the live lock while keeping its FD/path/token.
    // A real independent open now owns it, exposing validate_retained's
    // WouldBlock branch rather than an ordinary identity mismatch.
    guard.file.unlock().unwrap();
    let other = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    other.try_lock().unwrap();
    assert!(matches!(
        guard.file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(
        guard.validate_retained().await.is_err(),
        "WouldBlock must be propagated, never interpreted as a valid original guard"
    );
    drop(guard);
    let third = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(
        matches!(third.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "dropping a different FD cannot unlock the actual holder"
    );
    assert_eq!(std::fs::read(&path).unwrap(), OWNER.as_bytes());
    other.unlock().unwrap();
}

#[tokio::test]
async fn retained_guard_rejects_changed_receipt_marker_path_and_symlink() {
    for case in ["device", "inode", "executor", "marker", "path", "symlink"] {
        let root = tempfile::tempdir().unwrap();
        let (path, mut receipt) = retained(root.path());
        let mut executor = OWNER;
        match case {
            "device" => {
                if let UserAppOperationLeaseReceipt::Docker { device, .. } = &mut receipt {
                    *device += 1;
                }
            }
            "inode" => {
                if let UserAppOperationLeaseReceipt::Docker { inode, .. } = &mut receipt {
                    *inode += 1;
                }
            }
            "executor" => executor = "anotherexecutor",
            "marker" => std::fs::write(&path, b"anotherexecutor").unwrap(),
            "path" | "symlink" => {
                let old = root.path().join("original-inode");
                std::fs::rename(&path, &old).unwrap();
                if case == "path" {
                    std::fs::write(&path, OWNER).unwrap();
                } else {
                    std::os::unix::fs::symlink(&old, &path).unwrap();
                }
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(&path).unwrap();
        assert!(
            resume_original_builder_file(&path, &receipt, executor).is_err(),
            "{case}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{case}: no marker rewrite"
        );
    }
    for replace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (path, receipt) = retained(root.path());
        let guard = resume_original_builder_file(&path, &receipt, OWNER).unwrap();
        if replace {
            std::fs::rename(&path, root.path().join("old-inode")).unwrap();
            std::fs::write(&path, OWNER).unwrap();
        } else {
            std::fs::write(&path, b"anotherexecutor").unwrap();
        }
        assert!(!guard.validate_retained().await.unwrap());
        let marker = std::fs::read(&path).unwrap();
        drop(guard);
        assert_eq!(std::fs::read(&path).unwrap(), marker);
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

fn accept_with_deadline(listener: &UnixListener) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // macOS may inherit O_NONBLOCK from the listening socket.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "independent child failed to reach the barrier"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept independent child: {error}"),
        }
    }
}

#[test]
fn two_independent_processes_cannot_both_resume_the_original_guard() {
    if std::env::var_os("RCODER_RETAINED_GUARD_CHILD_PATH").is_some() {
        retained_original_guard_child();
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (path, receipt) = retained(root.path());
    let endpoint = root.path().join("guard.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    listener.set_nonblocking(true).unwrap();
    let spawn = || {
        ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CHILD_TEST, "--nocapture"])
                .env("RCODER_RETAINED_GUARD_CHILD_PATH", &path)
                .env("RCODER_RETAINED_GUARD_CHILD_SOCKET", &endpoint)
                .env(
                    "RCODER_RETAINED_GUARD_CHILD_RECEIPT",
                    serde_json::to_string(&receipt).unwrap(),
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    };
    let mut first = spawn();
    let mut second = spawn();
    assert_ne!(first.0.id(), second.0.id());
    let mut a = accept_with_deadline(&listener);
    let mut b = accept_with_deadline(&listener);
    let mut ready_a = [0; 1];
    let mut ready_b = [0; 1];
    a.read_exact(&mut ready_a).unwrap();
    b.read_exact(&mut ready_b).unwrap();
    assert_eq!(ready_a, [b'Y']);
    assert_eq!(ready_b, [b'Y']);
    a.write_all(b"G").unwrap();
    b.write_all(b"G").unwrap();
    let mut result_a = [0; 1];
    let mut result_b = [0; 1];
    a.read_exact(&mut result_a).unwrap();
    b.read_exact(&mut result_b).unwrap();
    let mut outcomes = [result_a[0], result_b[0]];
    outcomes.sort();
    assert_eq!(
        outcomes,
        [b'B', b'W'],
        "one kernel lock winner, one WouldBlock loser"
    );
    assert!(
        resume_original_builder_file(&path, &receipt, OWNER).is_err(),
        "the winning independent process still owns its original FD"
    );
    assert_eq!(std::fs::read(&path).unwrap(), OWNER.as_bytes());
    a.write_all(b"R").unwrap();
    b.write_all(b"R").unwrap();
    assert!(first.0.wait().unwrap().success());
    assert!(second.0.wait().unwrap().success());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        OWNER.as_bytes(),
        "both child exits preserve unknown state"
    );
    let final_guard = resume_original_builder_file(&path, &receipt, OWNER).unwrap();
    drop(final_guard);
}

fn retained_original_guard_child() {
    let path =
        std::path::PathBuf::from(std::env::var_os("RCODER_RETAINED_GUARD_CHILD_PATH").unwrap());
    let endpoint =
        std::path::PathBuf::from(std::env::var_os("RCODER_RETAINED_GUARD_CHILD_SOCKET").unwrap());
    let receipt: UserAppOperationLeaseReceipt =
        serde_json::from_str(&std::env::var("RCODER_RETAINED_GUARD_CHILD_RECEIPT").unwrap())
            .unwrap();
    let mut stream = UnixStream::connect(endpoint).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(b"Y").unwrap();
    let mut go = [0; 1];
    stream.read_exact(&mut go).unwrap();
    assert_eq!(go, [b'G']);
    let guard = match resume_original_builder_file(&path, &receipt, OWNER) {
        Ok(guard) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(runtime.block_on(guard.validate_retained()).unwrap());
            stream.write_all(b"W").unwrap();
            Some(guard)
        }
        Err(Error::Conflict(message)) => {
            assert!(
                message.contains("remains active"),
                "only actual lock contention may lose: {message}"
            );
            stream.write_all(b"B").unwrap();
            None
        }
        Err(error) => panic!("unexpected independent recovery error: {error}"),
    };
    let mut release = [0; 1];
    stream.read_exact(&mut release).unwrap();
    assert_eq!(release, [b'R']);
    drop(guard);
}

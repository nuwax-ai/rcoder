use super::*;

#[tokio::test]
async fn expired_file_caller_retains_real_writer_until_cleanup_observes_completion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("current.toml");
    std::fs::write(&path, b"original").unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let target = path.clone();
    let operation = tokio::spawn(async move {
        replace_file_with_until(
            target,
            b"replacement".to_vec(),
            tokio::time::Instant::now() + std::time::Duration::from_millis(500),
            move |target, bytes| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                durable_replace(target, bytes)
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), started)
        .await
        .unwrap()
        .unwrap();
    assert!(operation.await.unwrap().is_err());
    assert!(file_replacements_pending());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert!(
        wait_for_file_replacements_until(
            tokio::time::Instant::now() + std::time::Duration::from_millis(20)
        )
        .await
        .is_err()
    );
    release.send(()).unwrap();
    wait_for_file_replacements_until(
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(!file_replacements_pending());
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}

//! 日志目录命名和独占新建的单元测试，只用临时目录。

use super::*;

fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[test]
fn a_log_directory_is_named_after_its_container_and_the_utc_second() {
    assert_eq!(
        log_directory_name("rcoder-app-builder-265", at("2025-10-26T08:00:00Z")),
        "rcoder-app-builder-265-20251026080000"
    );
}

#[test]
fn a_replacement_keeps_the_owner_and_takes_a_new_timestamp() {
    assert_eq!(
        replacement_log_directory_name(
            "rcoder-app-builder-265-20251026080000",
            at("2025-10-26T08:00:10Z")
        ),
        "rcoder-app-builder-265-20251026080010"
    );
}

/// 换过名字的目录（`<容器名>-<时间>-<序号>`）和不像日志目录的名字：整个原名都当作前缀
#[test]
fn a_replacement_of_an_unfamiliar_name_appends_instead_of_replacing() {
    assert_eq!(
        replacement_log_directory_name(
            "rcoder-app-builder-265-20251026080000-2",
            at("2025-10-26T08:00:10Z")
        ),
        "rcoder-app-builder-265-20251026080000-2-20251026080010"
    );
    assert_eq!(
        replacement_log_directory_name("shared-logs", at("2025-10-26T08:00:10Z")),
        "shared-logs-20251026080010"
    );
    // 结尾是数字但不是 14 位时间戳
    assert_eq!(
        replacement_log_directory_name("runner-20251026", at("2025-10-26T08:00:10Z")),
        "runner-20251026-20251026080010"
    );
}

#[tokio::test]
async fn a_fresh_directory_is_created_under_its_preferred_name() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("logs").join("container");

    let created = create_fresh_directory(&parent, "runner-20251026080000")
        .await
        .unwrap();

    assert_eq!(created, parent.join("runner-20251026080000"));
    assert!(created.is_dir());
}

#[tokio::test]
async fn an_existing_directory_is_never_reused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("runner-20251026080000")).unwrap();
    std::fs::write(root.path().join("runner-20251026080000-2"), b"a file").unwrap();

    let created = create_fresh_directory(root.path(), "runner-20251026080000")
        .await
        .unwrap();

    assert_eq!(created, root.path().join("runner-20251026080000-3"));
    assert!(root.path().join("runner-20251026080000").is_dir());
    assert_eq!(
        std::fs::read(root.path().join("runner-20251026080000-2")).unwrap(),
        b"a file"
    );
}

#[tokio::test]
async fn every_taken_name_is_reported_instead_of_reusing_one() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("runner")).unwrap();
    for attempt in 2..=NAME_ATTEMPTS {
        std::fs::create_dir(root.path().join(format!("runner-{attempt}"))).unwrap();
    }

    let error = create_fresh_directory(root.path(), "runner")
        .await
        .unwrap_err();

    assert!(matches!(error, FreshDirectoryError::Exhausted));
}

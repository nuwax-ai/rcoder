//! Poison injection verifies the actual dev file lookup call chain.
use super::*;

#[tokio::test]
async fn config_budget_poisoned_dev_locator_returns_diagnostic_without_panicking() {
    use futures_util::FutureExt;
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(crate::test_support::MockRuntime::default());
    let service = crate::test_support::test_service(directory.path(), runtime.clone()).await;
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = service.dev_locator.write().unwrap();
        panic!("controlled locator poison");
    }));
    assert!(poisoned.is_err());
    let result =
        std::panic::AssertUnwindSafe(service.app_files_base(UserappStage::Dev, "app-original"))
            .catch_unwind()
            .await
            .expect("poisoned locator must not panic the file request");
    let error = result.expect_err("poison must return a concrete diagnostic");
    assert_eq!(error.code(), "ERR_RUNTIME_UNAVAILABLE");
    assert!(error.message().to_ascii_lowercase().contains("locator"));
    assert_eq!(
        runtime
            .ensure_workspace_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "unknown callback state must not switch to another authority"
    );
}

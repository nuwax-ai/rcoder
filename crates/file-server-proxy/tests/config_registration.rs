//! An admin start must use an explicitly registered configuration.

#[tokio::test]
async fn unregistered_proxy_does_not_start_with_public_defaults() {
    let result = file_server_proxy::try_start().await;
    if result.is_ok() {
        file_server_proxy::stop()
            .await
            .expect("stop unintended listener");
    }
    let error = result.expect_err("missing initialization must reject before opening a listener");
    assert!(
        error.contains("configuration has not been initialized"),
        "{error}"
    );
    assert!(file_server_proxy::status().await.is_none());
    // The same process can subsequently register its assembly failure. Neither
    // an admin retry nor Stop may discard that rejection.
    file_server_proxy::init_result(Err("invalid FILE_SERVER_PROXY_PUBLIC_BIND".into()));
    for _ in 0..2 {
        assert_eq!(
            file_server_proxy::try_start().await.unwrap_err(),
            "invalid FILE_SERVER_PROXY_PUBLIC_BIND"
        );
        assert!(file_server_proxy::status().await.is_none());
        file_server_proxy::stop().await.unwrap();
    }
}

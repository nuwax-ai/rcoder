use super::*;

#[tokio::test]
async fn ready_native_reply_after_deadline_cannot_publish_success() {
    let (send, receive) = oneshot::channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    send.send(Ok(42_u8)).unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let error = receive_reload_result(deadline, receive).await.unwrap_err();
    assert!(error.is::<crate::supervisor::ShutdownUnconfirmed>());
}

#[tokio::test]
async fn ready_native_failure_retains_its_known_outcome() {
    let (send, receive) = oneshot::channel::<Result<u8>>();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    send.send(Err(anyhow::anyhow!("candidate failed; cleanup confirmed")))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let error = receive_reload_result(deadline, receive).await.unwrap_err();
    assert!(!error.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(error.to_string().contains("cleanup confirmed"));
}

#[tokio::test]
async fn timely_native_reply_is_returned_and_closed_channel_remains_unknown() {
    let (send, receive) = oneshot::channel();
    send.send(Ok(42_u8)).unwrap();
    assert_eq!(
        receive_reload_result(
            tokio::time::Instant::now() + Duration::from_secs(1),
            receive
        )
        .await
        .unwrap(),
        42
    );
    let (send, receive) = oneshot::channel::<Result<u8>>();
    drop(send);
    assert!(
        receive_reload_result(
            tokio::time::Instant::now() + Duration::from_secs(1),
            receive
        )
        .await
        .unwrap_err()
        .is::<crate::supervisor::ShutdownUnconfirmed>()
    );
}

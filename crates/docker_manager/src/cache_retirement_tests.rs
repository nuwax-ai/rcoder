//! Real local Docker HTTP barriers; no Docker daemon or environment mutation.
use crate::{
    ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerManager, DockerManagerConfig,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn manager(address: std::net::SocketAddr) -> Arc<DockerManager> {
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    Arc::new(DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    })
}
async fn receive(listener: &tokio::net::TcpListener) -> (tokio::net::TcpStream, String) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = vec![];
    while !request.ends_with(b"\r\n\r\n") {
        request.push(stream.read_u8().await.unwrap());
    }
    (stream, String::from_utf8(request).unwrap())
}
async fn reply(mut stream: tokio::net::TcpStream, status: u16, body: &str) {
    stream.write_all(format!("HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}

#[tokio::test]
async fn late_inspect_cannot_repopulate_retired_status_or_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = manager(listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, request) = receive(&listener).await;
        assert!(request.contains("/containers/builder/json"));
        reply(
            stream,
            200,
            r#"{"Id":"old-id","Name":"/builder","State":{"Status":"running"}}"#,
        )
        .await;
        let (stream, request) = receive(&listener).await;
        assert!(request.contains("/containers/old-id/json"));
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        reply(
            stream,
            200,
            r#"{"Id":"old-id","NetworkSettings":{"Networks":{"test":{"IPAddress":"192.0.2.1"}}}}"#,
        )
        .await;
    });
    let reader = manager.clone();
    let query = tokio::spawn(async move { reader.find_container_authoritative("builder").await });
    ready_rx.await.unwrap();
    manager.retire_container_cache("old-id").await.unwrap();
    let replacement = Arc::new(ContainerQueryResult::new(
        "new-id".into(),
        "builder".into(),
        ContainerStatus::Running,
        true,
        "192.0.2.2".into(),
        chrono::Utc::now(),
    ));
    manager
        .api_cache
        .insert_status("builder".into(), Some(replacement))
        .await;
    release_tx.send(()).unwrap();
    // The in-flight caller still receives its actual inspect result.
    assert_eq!(
        query.await.unwrap().unwrap().unwrap().container_id,
        "old-id"
    );
    server.await.unwrap();
    assert!(manager.api_cache.get_status("old-id").await.is_none());
    assert!(manager.api_cache.get_network("old-id").await.is_none());
    assert_eq!(
        manager
            .api_cache
            .get_status("builder")
            .await
            .unwrap()
            .unwrap()
            .container_id,
        "new-id"
    );
}

#[tokio::test]
async fn late_network_not_found_cannot_publish_negative_cache_after_retirement() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = manager(listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = receive(&listener).await;
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        reply(stream, 404, r#"{"message":"not found"}"#).await;
    });
    let reader = manager.clone();
    let query = tokio::spawn(async move { reader.get_container_network_info("old-id").await });
    ready_rx.await.unwrap();
    manager.retire_container_cache("old-id").await.unwrap();
    release_tx.send(()).unwrap();
    assert!(query.await.unwrap().unwrap().is_empty());
    server.await.unwrap();
    assert!(manager.api_cache.get_network("old-id").await.is_none());
}

#[tokio::test]
async fn completed_old_stop_preserves_new_project_mapping() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = manager(listener.local_addr().unwrap());
    manager
        .containers
        .insert(
            "project".into(),
            DockerContainerInfo::new(
                "old-id".into(),
                "builder".into(),
                "project".into(),
                "image".into(),
            ),
        )
        .await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, request) = receive(&listener).await;
        assert!(request.contains("/containers/old-id/json"));
        reply(stream, 200, r#"{"Id":"old-id"}"#).await;
        let (stream, request) = receive(&listener).await;
        assert!(request.starts_with("DELETE ") && request.contains("/containers/old-id?"));
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        reply(stream, 204, "").await;
    });
    let stopper = manager.clone();
    let stop = tokio::spawn(async move { stopper.stop_container("project").await });
    ready_rx.await.unwrap();
    manager
        .containers
        .insert(
            "project".into(),
            DockerContainerInfo::new(
                "new-id".into(),
                "builder".into(),
                "project".into(),
                "image".into(),
            ),
        )
        .await;
    release_tx.send(()).unwrap();
    stop.await.unwrap().unwrap();
    server.await.unwrap();
    assert_eq!(
        manager
            .containers
            .get("project")
            .await
            .unwrap()
            .container_id,
        "new-id"
    );
}

#[tokio::test]
async fn late_status_not_found_preserves_new_alias_after_retirement() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = manager(listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = receive(&listener).await;
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        reply(stream, 404, r#"{"message":"not found"}"#).await;
    });
    let reader = manager.clone();
    let query = tokio::spawn(async move { reader.find_container_authoritative("builder").await });
    ready_rx.await.unwrap();
    manager.retire_container_cache("old-id").await.unwrap();
    manager
        .api_cache
        .insert_status(
            "builder".into(),
            Some(Arc::new(ContainerQueryResult::new(
                "new-id".into(),
                "builder".into(),
                ContainerStatus::Running,
                true,
                "192.0.2.2".into(),
                chrono::Utc::now(),
            ))),
        )
        .await;
    release_tx.send(()).unwrap();
    assert!(query.await.unwrap().unwrap().is_none());
    server.await.unwrap();
    assert_eq!(
        manager
            .api_cache
            .get_status("builder")
            .await
            .unwrap()
            .unwrap()
            .container_id,
        "new-id"
    );
}

async fn not_found_stop_retires_only_captured_identity(delete_not_found: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = manager(listener.local_addr().unwrap());
    let old_info = DockerContainerInfo::new(
        "old-id".into(),
        "builder".into(),
        "project".into(),
        "image".into(),
    );
    for alias in ["project", "old-alias"] {
        manager
            .containers
            .insert(alias.into(), old_info.clone())
            .await;
    }
    let old_query = Arc::new(ContainerQueryResult::new(
        "old-id".into(),
        "builder".into(),
        ContainerStatus::Running,
        true,
        "192.0.2.1".into(),
        chrono::Utc::now(),
    ));
    for alias in ["old-id", "builder"] {
        manager
            .api_cache
            .insert_status(alias.into(), Some(old_query.clone()))
            .await;
    }
    manager
        .api_cache
        .insert_network("old-id".into(), Some(Arc::new(Default::default())))
        .await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, request) = receive(&listener).await;
        assert!(request.contains("/containers/old-id/json"));
        if delete_not_found {
            reply(stream, 200, r#"{"Id":"old-id"}"#).await;
            let (delete_stream, request) = receive(&listener).await;
            assert!(request.starts_with("DELETE ") && request.contains("/containers/old-id?"));
            stream = delete_stream;
        }
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        reply(stream, 404, r#"{"message":"not found"}"#).await;
    });
    let stopper = manager.clone();
    let stop = tokio::spawn(async move { stopper.stop_container("project").await });
    ready_rx.await.unwrap();
    let mut replacement = old_info;
    replacement.container_id = "new-id".into();
    manager
        .containers
        .insert("project".into(), replacement)
        .await;
    manager
        .api_cache
        .insert_status(
            "builder".into(),
            Some(Arc::new(ContainerQueryResult::new(
                "new-id".into(),
                "builder".into(),
                ContainerStatus::Running,
                true,
                "192.0.2.2".into(),
                chrono::Utc::now(),
            ))),
        )
        .await;
    release_tx.send(()).unwrap();
    let result = stop.await.unwrap();
    server.await.unwrap();
    assert!(
        result.is_ok(),
        "already deleted container must stop idempotently: {result:?}"
    );
    assert!(
        manager.containers.get("old-alias").await.is_none(),
        "all aliases of the absent physical container must retire"
    );
    assert!(manager.api_cache.get_status("old-id").await.is_none());
    assert!(manager.api_cache.get_network("old-id").await.is_none());
    assert_eq!(
        manager
            .containers
            .get("project")
            .await
            .unwrap()
            .container_id,
        "new-id"
    );
    assert_eq!(
        manager
            .api_cache
            .get_status("builder")
            .await
            .unwrap()
            .unwrap()
            .container_id,
        "new-id"
    );
}

#[tokio::test]
async fn inspect_not_found_stop_retires_old_caches_and_preserves_replacement() {
    not_found_stop_retires_only_captured_identity(false).await;
}

#[tokio::test]
async fn delete_not_found_stop_retires_old_caches_and_preserves_replacement() {
    not_found_stop_retires_only_captured_identity(true).await;
}

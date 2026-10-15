//! Real PostgreSQL commits admission, then a relay drops its acknowledgement.
//! This distinguishes an unknown commit from a failed/uncommitted transaction.
use super::ToastyUserAppStore;
use crate::config::PostgresConfig;
use shared_types::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct CommitReplyRelay {
    port: u16,
    armed: Arc<AtomicBool>,
    observe_commits: Arc<AtomicBool>,
    commits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for CommitReplyRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl CommitReplyRelay {
    async fn start(host: String, port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_port = listener.local_addr().unwrap().port();
        let armed = Arc::new(AtomicBool::new(false));
        let observe_commits = Arc::new(AtomicBool::new(false));
        let commits = Arc::new(AtomicUsize::new(0));
        let (drop_commit, observing, count) =
            (armed.clone(), observe_commits.clone(), commits.clone());
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        let host = host.clone();
                        let (drop_commit, observing, count) = (drop_commit.clone(), observing.clone(), count.clone());
                        sessions.spawn(async move {
                            let server = TcpStream::connect((host.as_str(), port)).await.unwrap();
                            let (mut cr, mut cw) = client.into_split();
                            let (mut sr, mut sw) = server.into_split();
                            tokio::select! {
                                _ = tokio::io::copy(&mut cr, &mut sw) => {},
                                _ = async {
                                    loop {
                                        // sslmode=disable: every backend message is a typed
                                        // PG frame. CommandComplete("COMMIT") proves the
                                        // server finished the transaction before we close.
                                        let mut header = [0u8; 5];
                                        sr.read_exact(&mut header).await?;
                                        let length = u32::from_be_bytes(header[1..].try_into().unwrap());
                                        assert!((4..=1_048_576).contains(&length));
                                        let mut body = vec![0u8; (length - 4) as usize];
                                        sr.read_exact(&mut body).await?;
                                        if header[0] == b'C' && body == b"COMMIT\0" {
                                            if observing.load(Ordering::Acquire) { count.fetch_add(1, Ordering::AcqRel); }
                                            if drop_commit.swap(false, Ordering::AcqRel) { return Ok::<(), std::io::Error>(()); }
                                        }
                                        cw.write_all(&header).await?;
                                        cw.write_all(&body).await?;
                                    }
                                } => {},
                            }
                        });
                    },
                    Some(result) = sessions.join_next(), if !sessions.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            port: listen_port,
            armed,
            observe_commits,
            commits,
            task,
        }
    }
}

#[tokio::test]
#[ignore = "requires a disposable real PostgreSQL fault fixture"]
async fn committed_admission_reply_loss_preserves_original_identity_and_fence() {
    let dsn = crate::pg::test_support::test_dsn()
        .await
        .expect("disposable PG DSN required");
    let parsed: tokio_postgres::Config = dsn.parse().unwrap();
    let [tokio_postgres::config::Host::Tcp(host)] = parsed.get_hosts() else {
        panic!("test fixture requires one TCP host");
    };
    let relay = CommitReplyRelay::start(
        host.clone(),
        parsed.get_ports().first().copied().unwrap_or(5432),
    )
    .await;
    let mut proxied = PostgresConfig {
        host: Some("127.0.0.1".into()),
        port: Some(relay.port),
        username: parsed.get_user().map(str::to_owned),
        password: parsed
            .get_password()
            .map(|p| String::from_utf8(p.to_vec()).unwrap()),
        database: parsed.get_dbname().map(str::to_owned),
        max_connections: Some(1),
        min_connections: Some(1),
        connect_timeout_secs: Some(2),
        ..Default::default()
    };
    let application_name = format!("commitfault{}", uuid::Uuid::new_v4().simple());
    proxied.url = Some(format!(
        "{}?sslmode=disable&application_name={application_name}",
        proxied.to_dsn().unwrap()
    ));
    let a = ToastyUserAppStore::connect(&proxied).await.unwrap();
    let b = ToastyUserAppStore::connect(&PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    })
    .await
    .unwrap();
    let (observer, connection) = parsed.connect(tokio_postgres::NoTls).await.unwrap();
    let observer_task = tokio::spawn(connection);
    let app = a
        .ensure_identity(&format!("commit{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let old_pid: i32 = observer.query_one("SELECT pid FROM pg_stat_activity WHERE application_name=$1 AND datname=current_database()", &[&application_name]).await.unwrap().get(0);
    let input = UserAppExecutionInput::new("{\"fixture\":\"original-private-input\"}".into());
    let request = UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: uuid::Uuid::new_v4().simple().to_string(),
        request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
        request_fingerprint: input.digest(),
        kind: UserAppOperationKind::Create,
        command: Some(UserAppControlCommand::Create {
            input_digest: input.digest(),
        }),
        metadata: None,
        runtime_policy_on_success: None,
    };
    relay.observe_commits.store(true, Ordering::Release);
    relay.armed.store(true, Ordering::Release);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        a.admit_with_input(&request, Some(&input)),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "a lost commit reply must not claim successful acknowledgement"
    );
    assert!(
        !relay.armed.load(Ordering::Acquire),
        "the server actually completed COMMIT"
    );
    assert_eq!(
        relay.commits.load(Ordering::Acquire),
        1,
        "unknown commit must not be automatically retried"
    );
    relay.observe_commits.store(false, Ordering::Release);

    let original = b
        .get_operation_by_request(&app.app_id, request.request_id.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.operation_id, request.operation_id);
    assert_eq!(original.lifecycle_id, app.lifecycle_id);
    assert_eq!(original.state, UserAppOperationState::Pending);
    assert_eq!(
        b.get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .prod
            .as_deref(),
        Some(request.operation_id.as_str())
    );
    let row = observer.query_one("SELECT count(*),min(i.payload) FROM userapp_operations o JOIN userapp_requests r ON r.operation_id=o.operation_id AND r.app_id=o.app_id AND r.lifecycle_id=o.lifecycle_id JOIN userapp_operation_inputs i ON i.operation_id=o.operation_id AND i.app_id=o.app_id AND i.lifecycle_id=o.lifecycle_id WHERE o.app_id=$1 AND r.request_id=$2", &[&app.app_id, &request.request_id.as_deref().unwrap()]).await.unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, String>(1), input.encoded());
    let conflicting = UserAppAdmission {
        operation_id: uuid::Uuid::new_v4().simple().to_string(),
        request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
        kind: UserAppOperationKind::Stop,
        command: Some(UserAppControlCommand::Stop {
            wake_on_traffic: false,
        }),
        ..request.clone()
    };
    assert!(matches!(
        b.admit(&conflicting).await,
        Err(UserAppStoreError::OperationInProgress(_))
    ));
    // The failed connection must be discarded; the same owner reconnects and
    // replays the original request, never inventing a new operation or payload.
    let UserAppAdmissionOutcome::Existing(replayed) =
        a.admit_with_input(&request, Some(&input)).await.unwrap()
    else {
        panic!("same request must return its committed original");
    };
    assert_eq!(replayed, original);
    let count: i64 = observer
        .query_one(
            "SELECT count(*) FROM userapp_operations WHERE app_id=$1",
            &[&app.app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = observer
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid=$1 AND application_name=$2",
                    &[&old_pid, &application_name],
                )
                .await
                .unwrap()
                .get(0);
            if count == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("uncertain old connection must be physically closed");
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    drop(observer);
    observer_task.await.unwrap().unwrap();
    drop(relay);
}

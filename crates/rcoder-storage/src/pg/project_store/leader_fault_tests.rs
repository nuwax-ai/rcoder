//! Real dedicated-session closure under cancellation and transport failures.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Relay {
    port: u16,
    discard_responses: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relay {
    async fn start(host: String, port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_port = listener.local_addr().unwrap().port();
        let discard = Arc::new(AtomicBool::new(false));
        let controlled = discard.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        let host = host.clone();
                        let discard = controlled.clone();
                        connections.spawn(async move {
                            let upstream = TcpStream::connect((host.as_str(), port)).await.unwrap();
                            let (mut client_read, mut client_write) = client.into_split();
                            let (mut server_read, mut server_write) = upstream.into_split();
                            // EOF in either direction drops both sockets. A cancelled
                            // Toasty connection cannot leave a PG session in the relay.
                            tokio::select! {
                                _ = tokio::io::copy(&mut client_read, &mut server_write) => {},
                                _ = async {
                                    let mut bytes = [0u8; 8192];
                                    loop {
                                        let count = server_read.read(&mut bytes).await?;
                                        if count == 0 { break; }
                                        if !discard.load(Ordering::Acquire) {
                                            client_write.write_all(&bytes[..count]).await?;
                                        }
                                    }
                                    Ok::<_, std::io::Error>(())
                                } => {},
                            }
                        });
                    },
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    },
                }
            }
        });
        Self { port: listen_port, discard_responses: discard, task }
    }
}

async fn await_flag(election: &PgLeaderElection, expected: bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while election.is_leader() != expected {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("leadership must converge within real heartbeat budget");
}

#[tokio::test]
#[ignore = "requires the run-owned PostgreSQL fault fixture"]
async fn dedicated_leader_session_closes_after_shutdown_cancel_disconnect_and_timeout() {
    let dsn = crate::pg::test_support::test_dsn().await.expect("disposable PG DSN required");
    let parsed: tokio_postgres::Config = dsn.parse().unwrap();
    let [tokio_postgres::config::Host::Tcp(host)] = parsed.get_hosts() else {
        panic!("fault fixture must use exactly one TCP host");
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    let (observer, connection) = parsed.connect(tokio_postgres::NoTls).await.unwrap();
    let observer_task = tokio::spawn(connection);
    let initialized = crate::pg::test_support::database(&dsn).await;
    initialized.shutdown().await.unwrap();

    for mode in ["shutdown", "cancel_waiter", "disconnect", "response_timeout"] {
        let relay = Relay::start(host.clone(), port).await;
        let application_name = format!("leaderfault{}", uuid::Uuid::new_v4().simple());
        let mut config = PostgresConfig {
            host: Some("127.0.0.1".into()),
            port: Some(relay.port),
            username: parsed.get_user().map(str::to_owned),
            password: parsed.get_password().map(|bytes| String::from_utf8(bytes.to_vec()).unwrap()),
            database: parsed.get_dbname().map(str::to_owned),
            connect_timeout_secs: Some(2),
            ..Default::default()
        };
        config.url = Some(format!("{}?application_name={application_name}&sslmode=disable", config.to_dsn().unwrap()));
        let (shutdown, _) = broadcast::channel(1);
        let a = Arc::new(PgLeaderElection::spawn(config, shutdown.subscribe()));
        await_flag(&a, true).await;
        let rows = observer.query(
            "SELECT pid FROM pg_stat_activity WHERE application_name=$1 AND datname=current_database()",
            &[&application_name],
        ).await.unwrap();
        assert_eq!(rows.len(), 1, "the election owns one dedicated connection");
        let old_pid: i32 = rows[0].get(0);
        match mode {
            "shutdown" => a.shutdown().await.unwrap(),
            "cancel_waiter" => {
                let waiter_owner = a.clone();
                let waiter = tokio::spawn(async move { waiter_owner.shutdown().await });
                while !a._cancel.is_cancelled() { tokio::task::yield_now().await; }
                waiter.abort();
                let _ = waiter.await;
                a.shutdown().await.unwrap();
            },
            "disconnect" => {
                let killed = observer.query(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid=$1 AND application_name=$2 AND datname=current_database()",
                    &[&old_pid, &application_name],
                ).await.unwrap();
                assert_eq!(killed.len(), 1);
                assert!(killed[0].get::<_, bool>(0));
                await_flag(&a, false).await;
                a.shutdown().await.unwrap();
            },
            "response_timeout" => {
                relay.discard_responses.store(true, Ordering::Release);
                await_flag(&a, false).await;
                a.shutdown().await.unwrap();
            },
            _ => unreachable!(),
        }
        assert!(!a.is_leader());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = observer.query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid=$1 AND application_name=$2",
                    &[&old_pid, &application_name],
                ).await.unwrap().get(0);
                if count == 0 { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.expect("old dedicated backend must actually disappear");
        let b = PgLeaderElection::spawn(
            PostgresConfig { url: Some(dsn.clone()), ..Default::default() },
            shutdown.subscribe(),
        );
        await_flag(&b, true).await;
        assert!(!a.is_leader(), "old owner cannot regain leadership after close");
        b.shutdown().await.unwrap();
        drop(relay);
    }
    drop(observer);
    observer_task.await.unwrap().unwrap();
}

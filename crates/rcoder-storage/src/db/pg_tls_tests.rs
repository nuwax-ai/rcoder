//! Explicit real PG17 fixture; uses the production connector and owner unchanged.
use super::{postgres, schema::Component};
use crate::config::PostgresConfig;

fn configuration(name: &str) -> PostgresConfig {
    let url =
        std::env::var(name).expect("run tools/test_pg_storage_tls.py for isolated TLS inputs");
    let config = PostgresConfig {
        url: Some(url.clone()),
        max_connections: Some(1),
        min_connections: Some(1),
        connect_timeout_secs: Some(5),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    assert_eq!(
        config.to_dsn().unwrap(),
        url,
        "explicit TLS URL must be preserved"
    );
    config
}

#[tokio::test]
#[ignore = "requires tools/test_pg_storage_tls.py disposable PostgreSQL 17 TLS fixture"]
async fn postgres_verify_full_uses_tls_and_rejects_wrong_ca_and_hostname() {
    let good = configuration("RCODER_TLS_GOOD_DSN");
    let owner = postgres::open(&good, vec![Component::Userapp])
        .await
        .unwrap();
    owner.execute(|mut db| async move {
        // Query the exact checked-out backend, not another monitoring connection.
        let rows = toasty::sql::query(
            "SELECT CAST(pid AS TEXT) FROM pg_stat_ssl WHERE pid = pg_backend_pid() AND ssl AND version IS NOT NULL"
        ).exec(&mut db).await?;
        anyhow::ensure!(rows.len() == 1, "verify-full connection is not encrypted");
        let version = toasty::sql::query(
            "SELECT version() WHERE current_setting('server_version_num')::integer >= 170000 AND current_setting('server_version_num')::integer < 180000"
        ).exec(&mut db).await?;
        anyhow::ensure!(version.len() == 1, "fixture must run PostgreSQL 17");
        Ok(())
    }).await.unwrap();
    owner.shutdown().await.unwrap();

    // Plain TCP is intentionally available in this isolated fixture. These must
    // fail certificate verification rather than silently downgrade to that path.
    for name in ["RCODER_TLS_BAD_CA_DSN", "RCODER_TLS_BAD_HOST_DSN"] {
        let result = postgres::open(&configuration(name), vec![Component::Userapp]).await;
        if let Ok(owner) = result {
            owner.shutdown().await.unwrap();
            panic!("invalid verify-full identity was accepted");
        }
    }
    let plain = postgres::open(
        &configuration("RCODER_TLS_PLAIN_DSN"),
        vec![Component::Userapp],
    )
    .await
    .unwrap();
    plain
        .execute(|mut db| async move {
            let rows = toasty::sql::query(
            "SELECT CAST(pid AS TEXT) FROM pg_stat_ssl WHERE pid = pg_backend_pid() AND NOT ssl"
        ).exec(&mut db).await?;
            anyhow::ensure!(
                rows.len() == 1,
                "negative control must permit unencrypted transport"
            );
            Ok(())
        })
        .await
        .unwrap();
    plain.shutdown().await.unwrap();
}

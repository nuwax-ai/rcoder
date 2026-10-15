//! Run the same coordinator contract against the actual Toasty backend. This
//! explicit integration target requires a disposable PostgreSQL DSN.
#![cfg(all(test, feature = "pg"))]

#[tokio::test]
#[ignore = "requires RCODER_PG_TEST_DSN for a disposable PostgreSQL instance"]
async fn pg_preview_store_satisfies_contract() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("explicit PostgreSQL test DSN required");
    let mut admin = toasty::Db::builder()
        .connect(&dsn)
        .await
        .expect("connect admin");
    let schema = format!("preview_ct_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = crate::config::PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let store = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let tested = store.clone();
    let result =
        tokio::spawn(
            async move { preview_coordinator::contract_suite::run(tested.as_ref()).await },
        )
        .await;
    store.close().await.expect("drain preview database owner");
    // Only the schema just created by this test is ever removed. Cleanup also
    // runs when a contract assertion panics in the spawned test task.
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    result.expect("Preview PostgreSQL contract failed");
}

/// Promoted from tools/toasty-probe/src/owner_pg.rs::preview_contract.
/// Two independent database owners contend for missing rows and a shared port.
#[tokio::test]
#[ignore = "requires RCODER_PG_TEST_DSN for a disposable PostgreSQL instance"]
async fn independent_pg_preview_owners_serialize_creation_and_port_allocation() {
    use shared_types::{
        AcceptStartInput, AcceptStartOutcome, PreviewHostIdentity, PreviewLifecycleStore,
    };
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("explicit PostgreSQL test DSN required");
    let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
    let schema = format!("preview_race_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = crate::config::PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let a = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let b = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let (left_store, right_store) = (a.clone(), b.clone());
    let result = tokio::spawn(async move {
        let make = |key: &str, id: &str, requested_port| AcceptStartInput {
            preview_key: key.into(),
            project_id: "previewproject".into(),
            project_path: "/workspace/previewproject".into(),
            host: PreviewHostIdentity {
                host_id: "pod:boot".into(),
                pod_name: None,
                pod_ip: None,
            },
            operation_id: format!("operation{id}"),
            instance_id: format!("instance{id}"),
            requested_port,
            recover_unknown_evidence: None,
        };
        let (left, right) = tokio::join!(
            left_store.accept_start(make("samekey", "one", None)),
            right_store.accept_start(make("samekey", "two", None))
        );
        assert_eq!(
            usize::from(matches!(left, Ok(AcceptStartOutcome::Admitted(_))))
                + usize::from(matches!(right, Ok(AcceptStartOutcome::Admitted(_)))),
            1,
            "same-key initial start overwrote a concurrent winner"
        );
        let occupied = left_store.active_ports().await.unwrap();
        let port = (shared_types::PREVIEW_PORT_MIN..=shared_types::PREVIEW_PORT_MAX)
            .find(|p| shared_types::is_preview_port(*p) && !occupied.contains(p))
            .unwrap();
        let (left, right) = tokio::join!(
            left_store.accept_start(make("portkeyone", "three", Some(port))),
            right_store.accept_start(make("portkeytwo", "four", Some(port)))
        );
        assert_eq!(
            usize::from(left.is_ok()) + usize::from(right.is_ok()),
            1,
            "same port allocated twice"
        );
        assert_eq!(
            left_store
                .active_ports()
                .await
                .unwrap()
                .iter()
                .filter(|p| **p == port)
                .count(),
            1
        );
        assert_eq!(
            right_store
                .active_ports()
                .await
                .unwrap()
                .iter()
                .filter(|p| **p == port)
                .count(),
            1
        );
    })
    .await;
    let left_close = a.close().await;
    let right_close = b.close().await;
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    left_close.expect("left preview owner drain");
    right_close.expect("right preview owner drain");
    result.expect("Preview independent-owner contract failed");
}

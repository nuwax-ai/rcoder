//! Run the same coordinator contract against the actual Toasty backend. This
//! explicit integration target requires a disposable PostgreSQL DSN.
#![cfg(all(test, feature = "pg"))]

#[tokio::test]
#[ignore = "requires RCODER_PG_TEST_DSN for a disposable PostgreSQL instance"]
async fn pg_ready_base_completion_preserves_original_operation_receipt_and_activity() {
    use sha2::Digest as _;
    use shared_types::{
        AcceptStartInput, AcceptStartOutcome, PreviewHostIdentity, PreviewLifecycleStore,
    };
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("explicit PostgreSQL test DSN required");
    let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
    let schema = format!("preview_replay_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let scoped = format!("{dsn}{separator}options=-csearch_path%3D{schema}");
    let config = crate::config::PostgresConfig {
        url: Some(scoped.clone()),
        ..Default::default()
    };
    let store = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let peer = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let tested = store.clone();
    let tested_peer = peer.clone();
    let result = tokio::spawn(async move {
        let mut observer = toasty::Db::builder().connect(&scoped).await.unwrap();
        let AcceptStartOutcome::Admitted(starting) = tested
            .accept_start(AcceptStartInput {
                preview_key: "receipt-preview".into(),
                project_id: "receipt-project".into(),
                project_path: "/workspace/receipt-project".into(),
                host: PreviewHostIdentity {
                    host_id: "receipt-host:boot".into(),
                    pod_name: None,
                    pod_ip: None,
                },
                operation_id: "receipt-start".into(),
                instance_id: "receipt-instance".into(),
                requested_port: None,
                base_path: None,
                recover_unknown_evidence: None,
            })
            .await
            .unwrap()
        else {
            panic!("fresh start must be admitted")
        };
        let port = starting.port.unwrap();
        let ready = tested
            .publish_running(
                &starting.preview_key,
                &starting.operation_id,
                starting.revision,
                1234,
                port,
                None,
            )
            .await
            .unwrap();
        let receipt = operation_snapshot(&mut observer, &starting.operation_id).await;
        let legacy_request = serde_json::json!({
            "preview_key":"receipt-preview", "project_id":"receipt-project", "project_path":"/workspace/receipt-project",
            "instance_id":"receipt-instance", "host_id":"receipt-host:boot", "pod_name":null, "pod_ip":null,
            "requested_port":null, "recovery":null
        });
        let legacy_bytes = shared_types::encode_userapp_intent(&legacy_request).unwrap();
        let legacy_fingerprint = sha2::Sha256::digest(&legacy_bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        assert_eq!(receipt["request_fingerprint"].as_str(), Some(legacy_fingerprint.as_str()), "None base must preserve the legacy request fingerprint");
        assert_eq!(serde_json::from_str::<serde_json::Value>(receipt["request_json"].as_str().unwrap()).unwrap(), legacy_request);
        let (left, right) = tokio::join!(
            tested.publish_running(&starting.preview_key, &starting.operation_id, starting.revision, 1234, port, None),
            tested_peer.publish_running(&starting.preview_key, &starting.operation_id, starting.revision, 1234, port, None)
        );
        assert_eq!(left.unwrap(), ready);
        assert_eq!(right.unwrap(), ready);
        assert_eq!(operation_snapshot(&mut observer, &starting.operation_id).await, receipt);
        let input = AcceptStartInput {
            preview_key: "fingerprint-preview".into(), project_id: "fingerprint-project".into(), project_path: "/workspace/fingerprint-project".into(),
            host: PreviewHostIdentity { host_id: "receipt-host:boot".into(), pod_name: None, pod_ip: None },
            operation_id: "fingerprint-start".into(), instance_id: "fingerprint-instance".into(), requested_port: None,
            base_path: Some("/admitted/".into()), recover_unknown_evidence: None,
        };
        let AcceptStartOutcome::Admitted(admitted) = tested.accept_start(input.clone()).await.unwrap() else {
            panic!("new custom-base operation must be admitted");
        };
        assert_eq!(admitted.base_path.as_deref(), Some("/admitted/"));
        let custom_receipt = operation_snapshot(&mut observer, &input.operation_id).await;
        let custom_request: serde_json::Value = serde_json::from_str(custom_receipt["request_json"].as_str().unwrap()).unwrap();
        assert_eq!(custom_request["base_path"], "/admitted/");
        assert!(matches!(tested.accept_start(input.clone()).await, Ok(AcceptStartOutcome::Blocked(_))));
        let mut changed_input = input;
        changed_input.base_path = Some("/different/".into());
        assert!(matches!(tested.accept_start(changed_input).await, Err(shared_types::PreviewStoreError::Conflict(_))));
        assert_eq!(tested.get(&admitted.preview_key).await.unwrap().unwrap(), admitted);
        assert_eq!(operation_snapshot(&mut observer, &admitted.operation_id).await, custom_receipt);
        let completed = tested
            .publish_running(
                &starting.preview_key,
                &starting.operation_id,
                starting.revision,
                1234,
                port,
                Some("/custom/"),
            )
            .await
            .expect("complete base after heartbeat publication");
        assert_eq!(completed.base_path.as_deref(), Some("/custom/"));
        assert_eq!(completed.last_activity_at, ready.last_activity_at);
        assert_eq!(completed.last_heartbeat_at, ready.last_heartbeat_at);
        assert_eq!(
            operation_snapshot(&mut observer, &starting.operation_id).await,
            receipt,
            "base completion cannot rewrite the original receipt"
        );
        assert_eq!(
            tested
                .publish_running(
                    &starting.preview_key,
                    &starting.operation_id,
                    starting.revision,
                    1234,
                    port,
                    None
                )
                .await
                .unwrap(),
            completed
        );
        assert_eq!(
            operation_snapshot(&mut observer, &starting.operation_id).await,
            receipt
        );
        let (publication, stopping) = tokio::join!(
            tested.publish_running(&starting.preview_key, &starting.operation_id, starting.revision, 1234, port, None),
            tested_peer.accept_stop(&starting.preview_key, "receipt-stop")
        );
        match publication {
            Ok(row) => assert_eq!(row, completed),
            Err(shared_types::PreviewStoreError::Conflict(_)) => {},
            Err(error) => panic!("unexpected concurrent publication failure: {error}"),
        }
        let stopping = stopping.unwrap();
        assert_eq!(tested.get(&starting.preview_key).await.unwrap().unwrap(), stopping);
        assert!(matches!(tested.publish_running(&starting.preview_key, &starting.operation_id, starting.revision, 1234, port, None).await, Err(shared_types::PreviewStoreError::Conflict(_))));
        assert_eq!(tested.get(&starting.preview_key).await.unwrap().unwrap(), stopping);
        assert_eq!(operation_snapshot(&mut observer, &starting.operation_id).await, receipt);
    })
    .await;
    store.close().await.expect("drain preview database owner");
    peer.close()
        .await
        .expect("drain peer preview database owner");
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    result.expect("Preview PostgreSQL publication replay contract failed");
}

async fn operation_snapshot(db: &mut toasty::Db, operation_id: &str) -> serde_json::Value {
    let rows = toasty::sql::query(
        "SELECT row_to_json(p)::text FROM preview_operations p WHERE operation_id=$1",
    )
    .bind(operation_id)
    .exec(db)
    .await
    .unwrap();
    let [toasty_core::stmt::Value::Record(record)] = rows.as_slice() else {
        panic!("one operation receipt must exist");
    };
    let [toasty_core::stmt::Value::String(value)] = record.fields.as_slice() else {
        panic!("operation receipt must be returned as JSON text");
    };
    serde_json::from_str(value).unwrap()
}

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
            base_path: None,
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

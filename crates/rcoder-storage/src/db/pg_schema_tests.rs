//! Real PG baseline rejection and atomic DDL rollback in private test schemas.
use super::{postgres, schema::Component};
use crate::config::PostgresConfig;

#[tokio::test]
#[ignore = "requires a disposable superuser PostgreSQL fixture"]
async fn pg_baseline_rejects_tampering_and_rolls_back_partial_ddl() {
    let dsn = crate::pg::test_support::test_dsn().await.unwrap();
    let (client, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    for mode in ["checksum", "future", "ddl"] {
        let schema = format!("baseline{}", uuid::Uuid::new_v4().simple());
        client
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let separator = if dsn.contains('?') { '&' } else { '?' };
        let config = PostgresConfig {
            url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
            ..Default::default()
        };
        if mode == "ddl" {
            // Server-side fault only in this private schema. The trigger observes
            // actual earlier DDL before aborting CREATE TABLE userapp_operations.
            let trigger = format!("fault{schema}");
            client
                .batch_execute(&format!(
                    r#"
                CREATE FUNCTION {schema}.fail_partial() RETURNS event_trigger
                LANGUAGE plpgsql AS $$ BEGIN
                  IF current_schema() = '{schema}'
                     AND to_regclass('{schema}.userapp_operations') IS NOT NULL THEN
                    IF to_regclass('{schema}.userapps') IS NULL OR
                       to_regclass('{schema}.rcoder_schema_migrations') IS NULL THEN
                      RAISE EXCEPTION 'fixture did not observe prior DDL';
                    END IF;
                    RAISE EXCEPTION 'owned partial baseline fault';
                  END IF;
                END $$;
                CREATE EVENT TRIGGER {trigger} ON ddl_command_end
                WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION {schema}.fail_partial();
            "#
                ))
                .await
                .unwrap();
            let error = match postgres::open(&config, vec![Component::Userapp]).await {
                Err(error) => error,
                Ok(owner) => {
                    owner.shutdown().await.unwrap();
                    panic!("partial DDL unexpectedly committed");
                }
            };
            // Diagnostic assertion checks the injected failure, not production routing.
            assert!(format!("{error:#}").contains("owned partial baseline fault"));
            let count: i64 = client
                .query_one(
                    "SELECT count(*) FROM pg_tables WHERE schemaname=$1",
                    &[&schema],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 0, "ledger and earlier tables must all roll back");
            client
                .batch_execute(&format!("DROP EVENT TRIGGER {trigger}"))
                .await
                .unwrap();
            let owner = postgres::open(&config, vec![Component::Userapp])
                .await
                .unwrap();
            owner.shutdown().await.unwrap();
            let count: i64 = client.query_one(
                &format!("SELECT count(*) FROM {schema}.rcoder_schema_migrations WHERE component='userapp' AND version=1"), &[]
            ).await.unwrap().get(0);
            assert_eq!(count, 1, "retry installs exactly one complete baseline");
        } else {
            let owner = postgres::open(&config, vec![Component::Userapp])
                .await
                .unwrap();
            owner.shutdown().await.unwrap();
            let change = if mode == "checksum" {
                "checksum='tampered'"
            } else {
                "version=version+100"
            };
            client
                .batch_execute(&format!(
                    "UPDATE {schema}.rcoder_schema_migrations SET {change}"
                ))
                .await
                .unwrap();
            let query = format!(
                "SELECT row_to_json(m)::text FROM {schema}.rcoder_schema_migrations m ORDER BY component,version"
            );
            let before: Vec<String> = client
                .query(&query, &[])
                .await
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            match postgres::open(&config, vec![Component::Userapp]).await {
                Err(_) => {}
                Ok(owner) => {
                    owner.shutdown().await.unwrap();
                    panic!("accepted {mode} baseline");
                }
            }
            let after: Vec<String> = client
                .query(&query, &[])
                .await
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(before, after, "rejection cannot repair or rewrite evidence");
        }
        client
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable superuser PostgreSQL fixture"]
async fn compute_pg_v1_upgrade_preserves_existing_lifecycle_and_baseline() {
    let dsn = crate::pg::test_support::test_dsn()
        .await
        .expect("disposable PostgreSQL required");
    let (client, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let schema = format!("computeupgrade{}", uuid::Uuid::new_v4().simple());
    client
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let owner = postgres::open(&config, vec![Component::Userapp])
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    client.batch_execute(&format!("SET search_path TO {schema}; DROP TABLE userapp_compute_controls; DROP TABLE userapp_compute_intents; DELETE FROM rcoder_schema_migrations WHERE component='userapp' AND version=2; INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us) VALUES('keptapp','keptlife',1,'active',1,1,1); INSERT INTO userapp_active_operations(app_id,lifecycle_id) VALUES('keptapp','keptlife');")).await.unwrap();
    let query = "SELECT checksum,schema_fingerprint FROM rcoder_schema_migrations WHERE component='userapp' AND version=1";
    let before = client.query_one(query, &[]).await.unwrap();
    let before: (String, String) = (before.get(0), before.get(1));
    for _ in 0..2 {
        let owner = postgres::open(&config, vec![Component::Userapp])
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
    }
    let after = client.query_one(query, &[]).await.unwrap();
    assert_eq!(before, (after.get(0), after.get(1)));
    let app = client
        .query_one(
            "SELECT lifecycle_id FROM userapps WHERE app_id='keptapp'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(app.get::<_, String>(0), "keptlife");
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM rcoder_schema_migrations WHERE component='userapp'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 2);
    client
        .batch_execute(&format!(
            "SET search_path TO public; DROP SCHEMA {schema} CASCADE"
        ))
        .await
        .unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}

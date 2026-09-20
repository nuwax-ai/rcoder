//! S01: valid operation IDs belonging to another application or lifecycle must
//! not be accepted as private input, physical lease, or active-slot references.
use super::owner::DatabaseOwner;

async fn contract(owner: &DatabaseOwner) {
    owner.execute(|mut db| async move {
        let fingerprint = "a".repeat(64);
        for (app, life) in [("appone", "lifeone"), ("apptwo", "lifetwo")] {
            toasty::sql::statement(format!("INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us) VALUES('{app}','{life}',1,'active',1,1,1)"))
                .exec(&mut db).await?;
        }
        // Historical operations intentionally survive lifecycle replacement.
        for (op, app, life) in [("currentone", "appone", "lifeone"), ("currenttwo", "apptwo", "lifetwo"), ("historicalone", "appone", "oldlife")] {
            toasty::sql::statement(format!("INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,step,payload_version,checkpoint_json,created_at_us,updated_at_us) VALUES('{op}','{app}','{life}','start','prod','pending',1,'{fingerprint}','accepted',1,'null',1,1)"))
                .exec(&mut db).await?;
        }
        toasty::sql::statement("INSERT INTO userapp_active_operations(app_id,lifecycle_id,prod_operation_id) VALUES('appone','lifeone','currentone')").exec(&mut db).await?;
        for foreign in ["currenttwo", "historicalone"] {
            for table in ["input", "lease", "slot"] {
                let sql = match table {
                    "input" => format!("INSERT INTO userapp_operation_inputs(operation_id,app_id,lifecycle_id,payload_version,payload,payload_digest,created_at_us) VALUES('{foreign}','appone','lifeone',1,'privatefixture','{fingerprint}',1)"),
                    "lease" => format!("INSERT INTO userapp_operation_leases(operation_id,app_id,lifecycle_id,executor_id,request_fingerprint,receipt_version,receipt_json,created_at_us) VALUES('{foreign}','appone','lifeone','executor','{fingerprint}',1,'{{}}',1)"),
                    "slot" => format!("UPDATE userapp_active_operations SET prod_operation_id='{foreign}' WHERE app_id='appone'"),
                    _ => unreachable!(),
                };
                let mut tx = db.transaction().await?;
                // A preceding valid mutation must roll back with the bad reference.
                toasty::sql::statement("UPDATE userapps SET name='partial' WHERE app_id='appone'").exec(&mut tx).await?;
                assert!(toasty::sql::statement(sql).exec(&mut tx).await.is_err(), "{table} accepted foreign operation {foreign}");
                tx.rollback().await?;
                assert!(toasty::sql::query("SELECT app_id FROM userapps WHERE name IS NOT NULL").exec(&mut db).await?.is_empty());
                assert!(toasty::sql::query("SELECT operation_id FROM userapp_operation_inputs").exec(&mut db).await?.is_empty());
                assert!(toasty::sql::query("SELECT operation_id FROM userapp_operation_leases").exec(&mut db).await?.is_empty());
                assert_eq!(toasty::sql::query("SELECT app_id FROM userapp_active_operations WHERE app_id='appone' AND lifecycle_id='lifeone' AND prod_operation_id='currentone'").exec(&mut db).await?.len(), 1);
            }
        }
        // Positive controls prove failures are identity constraints, not unusable SQL.
        toasty::sql::statement(format!("INSERT INTO userapp_operation_inputs(operation_id,app_id,lifecycle_id,payload_version,payload,payload_digest,created_at_us) VALUES('currentone','appone','lifeone',1,'privatefixture','{fingerprint}',1)")).exec(&mut db).await?;
        toasty::sql::statement(format!("INSERT INTO userapp_operation_leases(operation_id,app_id,lifecycle_id,executor_id,request_fingerprint,receipt_version,receipt_json,created_at_us) VALUES('currentone','appone','lifeone','executor','{fingerprint}',1,'{{}}',1)")).exec(&mut db).await?;
        assert_eq!(toasty::sql::statement("UPDATE userapp_active_operations SET prod_operation_id='currentone' WHERE app_id='appone'").exec(&mut db).await?, 1);
        Ok(())
    }).await.unwrap();
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_rejects_cross_identity_input_lease_and_slot_references() {
    use super::{
        driver::{ConnectionPolicy, PolicyDriver},
        models,
        schema::{self, Backend, Component},
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("identity.db");
    let owner = DatabaseOwner::open(8, 1, (), move || async move {
        let driver = PolicyDriver::new(
            toasty_driver_turso::Turso::file(&file),
            ConnectionPolicy::Turso,
        );
        let mut db = toasty::Db::builder()
            .models(models::storage_models())
            .max_pool_size(1)
            .build(driver)
            .await?;
        schema::initialize(&mut db, Backend::Turso, &[Component::Userapp]).await?;
        Ok(db)
    })
    .await
    .unwrap();
    contract(&owner).await;
    owner.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires isolated RCODER_PG_TEST_DSN"]
async fn pg_rejects_cross_identity_input_lease_and_slot_references() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("isolated PG DSN required");
    let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
    let schema = format!("identity_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = crate::config::PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let owner = super::postgres::open(&config, vec![super::schema::Component::Userapp])
        .await
        .unwrap();
    let worker = owner.clone();
    let result = tokio::spawn(async move { contract(&worker).await }).await;
    let closed = owner.shutdown().await;
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    closed.unwrap();
    result.unwrap();
}

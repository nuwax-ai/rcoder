#[cfg(feature = "common-contract")]
#[path = "../../../crates/rcoder-storage/src/config.rs"]
pub(crate) mod config;
#[cfg(feature = "common-contract")]
#[path = "../../../crates/rcoder-storage/src/db/postgres.rs"]
pub(crate) mod postgres_connector;
#[cfg(feature = "common-contract")]
mod preview_lifecycle;
#[cfg(feature = "common-contract")]
mod userapp_lifecycle;
#[cfg(feature = "common-contract")]
mod db {
    pub(crate) use crate::postgres_connector as postgres;
    pub(crate) use crate::{
        owner, policy_driver as driver, storage_models as models, storage_schema as schema,
    };
}
mod managed;
#[path = "../../../crates/rcoder-storage/src/db/owner.rs"]
pub(crate) mod owner;
mod owner_pg;
#[cfg(test)]
#[path = "../../../crates/rcoder-storage/src/db/tests.rs"]
mod owner_tests;
#[cfg(test)]
use policy_driver as driver;
#[cfg(test)]
use storage_models as models;
#[cfg(test)]
use storage_schema as schema;
#[path = "../../../crates/rcoder-storage/src/db/driver.rs"]
pub(crate) mod policy_driver;
#[path = "../../../crates/rcoder-storage/src/db/models.rs"]
pub(crate) mod storage_models;
#[path = "../../../crates/rcoder-storage/src/db/schema.rs"]
pub(crate) mod storage_schema;
use anyhow::{Result, ensure};
#[derive(Debug, toasty::Model)]
struct Probe {
    #[key]
    id: i64,
    revision: i64,
    value: String,
}
fn one_bool(rows: &[toasty_core::stmt::Value]) -> Result<bool> {
    let [toasty_core::stmt::Value::Record(record)] = rows else {
        anyhow::bail!("expected one SQL record");
    };
    match record.fields.as_slice() {
        [toasty_core::stmt::Value::Bool(value)] => Ok(*value),
        _ => anyhow::bail!("expected one boolean SQL column"),
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        std::env::var("RCODER_TOASTY_PROBE_DISPOSABLE").as_deref() == Ok("1"),
        "set RCODER_TOASTY_PROBE_DISPOSABLE=1 only for a new, empty disposable database"
    );
    let path = std::env::var("RCODER_TOASTY_PROBE_URL")?;
    #[cfg(feature = "common-contract")]
    if std::env::var("RCODER_TOASTY_PREVIEW_PG_PROBE").as_deref() == Ok("1") {
        return owner_pg::preview_contract(path).await;
    }
    if std::env::var("RCODER_TOASTY_SCHEMA_PROBE").as_deref() == Ok("1") {
        return owner_pg::schema(path).await;
    }
    if std::env::var("RCODER_TOASTY_OWNER_PROBE").as_deref() == Ok("1") {
        return owner_pg::run(path).await;
    }
    #[cfg(feature = "common-contract")]
    if std::env::var("RCODER_TOASTY_COMMON_PG_PROBE").as_deref() == Ok("1") {
        return owner_pg::common_contract(path).await;
    }
    let postgres = path.starts_with("postgres");
    let owned_pg = if postgres {
        let config: tokio_postgres::Config = path.parse()?;
        ensure!(
            config.get_ssl_mode() == tokio_postgres::config::SslMode::Disable,
            "this isolated transport probe requires explicit sslmode=disable; never use it as a production connector"
        );
        Some(config)
    } else {
        None
    };
    let (driver, closed) =
        managed::TrackedDriver::new(toasty::db::Connect::new(&path).await?, owned_pg);
    let mut db = toasty::Db::builder()
        .models(toasty::models!(Probe))
        .max_pool_size(1)
        .build(driver)
        .await?;
    db.push_schema().await?; // disposable probe database only
    Probe::create()
        .id(1)
        .revision(0)
        .value("old")
        .exec(&mut db)
        .await?;
    let row = Probe::get_by_id(&mut db, 1).await?;
    ensure!(row.revision == 0 && row.value == "old");
    if !postgres {
        toasty::sql::statement("PRAGMA foreign_keys=ON")
            .exec(&mut db)
            .await?;
        println!(
            "foreign_keys={:?}",
            toasty::sql::query("PRAGMA foreign_keys")
                .exec(&mut db)
                .await?
        );
    }
    toasty::sql::statement("CREATE TABLE t0_parent (id TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision >= 0))").exec(&mut db).await?;
    toasty::sql::statement("CREATE TABLE t0_child (id TEXT PRIMARY KEY, parent TEXT NOT NULL REFERENCES t0_parent(id))").exec(&mut db).await?;
    ensure!(
        toasty::sql::statement("INSERT INTO t0_child VALUES ('orphan', 'missing')")
            .exec(&mut db)
            .await
            .is_err(),
        "foreign key not enforced"
    );
    ensure!(
        toasty::sql::statement("INSERT INTO t0_parent VALUES ('bad', -1)")
            .exec(&mut db)
            .await
            .is_err(),
        "CHECK not enforced"
    );
    toasty::sql::statement("INSERT INTO t0_parent VALUES ('app', 0)")
        .exec(&mut db)
        .await?;
    for expected in [1, 0] {
        let changed = toasty::sql::statement(if postgres {
            "UPDATE t0_parent SET revision=1 WHERE id=$1 AND revision=$2"
        } else {
            "UPDATE t0_parent SET revision=1 WHERE id=?1 AND revision=?2"
        })
        .bind("app")
        .bind(0_i64)
        .exec(&mut db)
        .await?;
        ensure!(
            changed == expected,
            "CAS affected rows {changed}, expected {expected}"
        );
    }
    let mut tx = db.transaction().await?;
    toasty::sql::statement("INSERT INTO t0_child VALUES ('rolledback', 'app')")
        .exec(&mut tx)
        .await?;
    tx.rollback().await?;
    ensure!(
        toasty::sql::query("SELECT id FROM t0_child")
            .exec(&mut db)
            .await?
            .is_empty()
    );
    if postgres {
        let mut leader = db.connection().await?;
        toasty::sql::statement("SELECT pg_advisory_lock(123654789)")
            .exec(&mut leader)
            .await?;
        let mut rival = toasty::Db::builder().connect(&path).await?;
        let locked = toasty::sql::query("SELECT pg_try_advisory_lock(123654789)")
            .exec(&mut rival)
            .await?;
        ensure!(!one_bool(&locked)?);
        drop(db);
        ensure!(
            !closed.borrow().driver_dropped,
            "driver dropped while leader connection held"
        );
        drop(leader);
        managed::wait_closed(closed).await?;
        // Local transport completion is not evidence that the server has
        // processed TCP close. Only the rival's real lock acquisition authorizes
        // leadership; a bounded observation loop is not a sleep-based unlock.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let locked = toasty::sql::query("SELECT pg_try_advisory_lock(123654789)")
                    .exec(&mut rival)
                    .await?;
                if one_bool(&locked)? {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        toasty::sql::query("SELECT pg_advisory_unlock(123654789)")
            .exec(&mut rival)
            .await?;
    } else {
        drop(db);
        managed::wait_closed(closed).await?;
    }
    println!("PASS model roundtrip, FK, CHECK, CAS, rollback, tracked physical closure");
    Ok(())
}

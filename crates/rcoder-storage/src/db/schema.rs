//! Explicit, checksummed first-release baselines. Never adopt an old development
//! database or use Toasty's destructive schema reset/push in production.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};
use toasty::db::Transaction;
use toasty_core::{driver::operation::TransactionMode, stmt::Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Postgres,
    Turso,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Component {
    Userapp,
    Project,
    Preview,
}
impl Component {
    fn name(self) -> &'static str {
        match self {
            Self::Userapp => "userapp",
            Self::Project => "project",
            Self::Preview => "preview",
        }
    }
    fn ddl(self, backend: Backend) -> Result<&'static str> {
        match (self, backend) {
            (Self::Userapp, Backend::Postgres) => {
                Ok(include_str!("../../schema/userapp-pg-v1.sql"))
            }
            (Self::Userapp, Backend::Turso) => {
                Ok(include_str!("../../schema/userapp-turso-v1.sql"))
            }
            (Self::Project, Backend::Postgres) => {
                Ok(include_str!("../../schema/project-pg-v1.sql"))
            }
            (Self::Preview, Backend::Postgres) => {
                Ok(include_str!("../../schema/preview-pg-v1.sql"))
            }
            _ => bail!("requested database component is unsupported by this backend"),
        }
    }
}

// Only our four fixed DDL files use this splitter. Their grammar excludes SQL
// function bodies and semicolons in literals; reject those instead of guessing.
fn statements(ddl: &str) -> Result<Vec<String>> {
    let ddl = ddl
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut quoted = false;
    for ch in ddl.chars() {
        if ch == '\'' {
            quoted = !quoted;
        }
        ensure!(
            ch != ';' || !quoted,
            "baseline splitter does not support semicolons in literals"
        );
    }
    ensure!(
        !quoted && !ddl.contains("$$") && !ddl.contains("/*"),
        "unsupported baseline SQL grammar"
    );
    Ok(ddl
        .split(';')
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
        .map(str::to_owned)
        .collect())
}
fn tables(ddl: &str) -> Result<BTreeSet<String>> {
    let mut tables = BTreeSet::new();
    for statement in statements(ddl)? {
        if let Some(rest) = statement.strip_prefix("CREATE TABLE ") {
            let name = rest
                .split_whitespace()
                .next()
                .context("missing table name")?;
            ensure!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "invalid fixed baseline identifier"
            );
            ensure!(tables.insert(name.to_owned()), "duplicate baseline table");
        }
    }
    Ok(tables)
}
fn digest(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn string_rows(rows: Vec<Value>) -> Result<Vec<Vec<String>>> {
    rows.into_iter()
        .map(|row| {
            let Value::Record(row) = row else {
                bail!("expected schema catalog record");
            };
            row.fields
                .into_iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value),
                    _ => bail!("unexpected schema catalog column type"),
                })
                .collect()
        })
        .collect()
}

pub(crate) async fn initialize(
    db: &mut toasty::Db,
    backend: Backend,
    enabled: &[Component],
) -> Result<()> {
    ensure!(
        !enabled.is_empty(),
        "database initialization requires an explicit component set"
    );
    let unique: BTreeSet<_> = enabled.iter().copied().collect();
    ensure!(
        unique.len() == enabled.len(),
        "duplicate database component"
    );
    for component in &unique {
        component.ddl(backend)?;
    }
    for component in unique {
        let mut builder = db.transaction_builder();
        if backend == Backend::Turso {
            builder = builder.mode(TransactionMode::Immediate);
        }
        let mut tx = builder.begin().await?;
        let result = initialize_component(&mut tx, backend, component, true).await;
        match result {
            Ok(()) => tx.commit().await.context("commit schema baseline")?,
            Err(error) => {
                if let Err(rollback) = tx.rollback().await {
                    return Err(error.context(format!(
                        "schema rollback failed; connection must be quarantined: {rollback}"
                    )));
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Verify the existing baseline without executing DDL or changing the ledger.
pub(crate) async fn verify_existing(
    db: &mut toasty::Db,
    backend: Backend,
    component: Component,
) -> Result<()> {
    let mut tx = db.transaction().await?;
    match initialize_component(&mut tx, backend, component, false).await {
        Ok(()) => tx.commit().await.context("finish schema verification"),
        Err(error) => {
            tx.rollback()
                .await
                .context("rollback schema verification")?;
            Err(error)
        }
    }
}

async fn initialize_component(
    tx: &mut Transaction<'_>,
    backend: Backend,
    component: Component,
    allow_create: bool,
) -> Result<()> {
    if backend == Backend::Postgres {
        // Same database-wide transaction lock for every domain and replica,
        // acquired BEFORE creating or inspecting the ledger.
        toasty::sql::statement("SELECT pg_advisory_xact_lock(7272636, 1)")
            .exec(&mut *tx)
            .await?;
    }
    let existing = catalog_tables(tx, backend).await?;
    let ledger_exists = existing.contains("rcoder_schema_migrations");
    if !ledger_exists {
        ensure!(
            allow_create,
            "Offline observer requires an initialized schema ledger"
        );
        ensure!(
            existing.is_empty(),
            "database contains unversioned/old development tables; explicit test-database baseline reset is required"
        );
        toasty::sql::statement("CREATE TABLE rcoder_schema_migrations (component TEXT NOT NULL, version BIGINT NOT NULL CHECK(version>=1), name TEXT NOT NULL, checksum TEXT NOT NULL, schema_fingerprint TEXT NOT NULL, applied_at_us BIGINT NOT NULL, PRIMARY KEY(component,version))").exec(&mut *tx).await?;
    }
    let rows = string_rows(toasty::sql::query("SELECT component, CAST(version AS TEXT), name, checksum, schema_fingerprint FROM rcoder_schema_migrations ORDER BY component,version").exec(&mut *tx).await?)?;
    let mut recorded = BTreeMap::new();
    let mut expected_tables = BTreeSet::from(["rcoder_schema_migrations".to_owned()]);
    for row in rows {
        let [name, version, migration_name, checksum, fingerprint] = row.as_slice() else {
            bail!("invalid migration ledger shape");
        };
        let known = match name.as_str() {
            "userapp" => Component::Userapp,
            "project" => Component::Project,
            "preview" => Component::Preview,
            _ => bail!("unknown database schema component"),
        };
        let ddl = known.ddl(backend)?;
        ensure!(
            version == "1" && migration_name == "baseline-v1",
            "unsupported/future database schema version"
        );
        ensure!(
            *checksum == digest(ddl),
            "database schema checksum differs from executable baseline"
        );
        let component_tables = tables(ddl)?;
        ensure!(
            component_tables.is_subset(&existing),
            "versioned database component has missing tables"
        );
        ensure!(
            *fingerprint == catalog_fingerprint(tx, backend, &component_tables).await?,
            "database component catalog differs from its installed constraints/columns/indexes"
        );
        expected_tables.extend(component_tables);
        ensure!(
            recorded.insert(known, fingerprint.clone()).is_none(),
            "duplicate baseline component"
        );
    }
    // Unknown and partly initialized tables are not silently adopted.
    ensure!(
        existing.is_subset(&expected_tables),
        "unexpected tables in RCoder schema"
    );
    if recorded.contains_key(&component) {
        return Ok(());
    }
    ensure!(
        allow_create,
        "Offline observer requires an installed component baseline"
    );
    let ddl = component.ddl(backend)?;
    for statement in statements(ddl)? {
        toasty::sql::statement(statement).exec(&mut *tx).await?;
    }
    let fingerprint = catalog_fingerprint(tx, backend, &tables(ddl)?).await?;
    let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())
        .context("schema timestamp overflow")?;
    let sql = match backend {
        Backend::Postgres => {
            "INSERT INTO rcoder_schema_migrations VALUES ($1,1,'baseline-v1',$2,$3,$4)"
        }
        Backend::Turso => {
            "INSERT INTO rcoder_schema_migrations VALUES (?1,1,'baseline-v1',?2,?3,?4)"
        }
    };
    toasty::sql::statement(sql)
        .bind(component.name())
        .bind(digest(ddl))
        .bind(fingerprint)
        .bind(now)
        .exec(&mut *tx)
        .await?;
    Ok(())
}

async fn catalog_tables(tx: &mut Transaction<'_>, backend: Backend) -> Result<BTreeSet<String>> {
    let sql = match backend {
        Backend::Postgres => {
            "SELECT tablename::text FROM pg_catalog.pg_tables WHERE schemaname=current_schema() ORDER BY tablename"
        }
        Backend::Turso => {
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        }
    };
    string_rows(toasty::sql::query(sql).exec(&mut *tx).await?)?
        .into_iter()
        .map(|row| {
            let [name] = row.as_slice() else {
                bail!("invalid table catalog shape");
            };
            Ok(name.clone())
        })
        .collect()
}
async fn catalog_fingerprint(
    tx: &mut Transaction<'_>,
    backend: Backend,
    tables: &BTreeSet<String>,
) -> Result<String> {
    // Names originate only in the fixed manifests and were validated by tables().
    let names = tables
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = match backend {
        Backend::Turso => format!(
            "SELECT type || ':' || name || ':' || COALESCE(sql,'') FROM sqlite_schema WHERE tbl_name IN ({names}) ORDER BY type,name"
        ),
        Backend::Postgres => format!(
            r#"SELECT definition FROM (
 SELECT c.relname || ':column:' || a.attnum::text || ':' || a.attname || ':' || pg_catalog.format_type(a.atttypid,a.atttypmod) || ':' || a.attnotnull::text || ':' || COALESCE(pg_get_expr(d.adbin,d.adrelid),'') AS definition
 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid
 LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
 WHERE n.nspname=current_schema() AND c.relname IN ({names}) AND a.attnum>0 AND NOT a.attisdropped
 UNION ALL SELECT c.relname || ':constraint:' || k.conname || ':' || pg_get_constraintdef(k.oid)
 FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname=current_schema() AND c.relname IN ({names})
 UNION ALL SELECT tablename || ':index:' || indexname || ':' || indexdef FROM pg_indexes
 WHERE schemaname=current_schema() AND tablename IN ({names})
 ) catalog ORDER BY definition"#
        ),
    };
    let rows = string_rows(toasty::sql::query(sql).exec(&mut *tx).await?)?;
    ensure!(!rows.is_empty(), "installed database catalog is empty");
    // Length framing prevents ambiguous delimiters in SQL expressions.
    let mut bytes = String::new();
    for row in rows {
        for value in row {
            bytes.push_str(&format!("{}:{value}", value.len()));
        }
    }
    Ok(digest(&bytes))
}

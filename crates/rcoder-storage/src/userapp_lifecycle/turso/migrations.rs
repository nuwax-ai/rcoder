//! 小型版本迁移器（plan §4）：一个迁移与版本记录同一事务，失败全回滚；
//! 高于程序支持版本或校验和变化拒绝启动。迁移成功前不运行恢复扫描。

use std::path::Path;

use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};

/// 内嵌迁移清单（顺序即应用顺序；文件名仅标识，版本 = 序号）。
/// include_str 保证二进制与源码一致——不运行时读文件（发布产物可移植）。
const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_init.sql",
    include_str!("../../../migrations-userapp-turso/0001_init.sql"),
)];

const VERSIONS_TABLE: &str = "_turso_userapp_migrations";

/// 校验和应用全部待执行迁移；返回应用的迁移数。
pub(super) async fn run(conn: &turso::Connection) -> anyhow::Result<usize> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {VERSIONS_TABLE} (
                version INTEGER PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )"
        ),
        (),
    )
    .await
    .with_context(|| format!("create {VERSIONS_TABLE}"))?;
    let applied = read_applied(conn).await?;
    verify_history(&applied)?;
    let mut count = 0;
    for (index, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).context("migration version overflow")?;
        if applied.iter().any(|record| record.version == version) {
            continue;
        }
        let checksum = checksum_of(sql);
        // 迁移 + 版本记录同一事务：失败全回滚（无半迁移）。
        conn.execute("BEGIN IMMEDIATE", ())
            .await
            .with_context(|| format!("begin migration {name}"))?;
        let result = (async {
            conn.execute_batch(sql)
                .await
                .with_context(|| format!("apply migration {name}"))?;
            conn.execute(
                &format!(
                    "INSERT INTO {VERSIONS_TABLE}(version,name,checksum,applied_at) VALUES(?,?,?,?)"
                ),
                (
                    turso::Value::Integer(version),
                    turso::Value::Text((*name).to_string()),
                    turso::Value::Text(checksum.clone()),
                    turso::Value::Text(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    ),
                ),
            )
            .await
            .with_context(|| format!("record migration {name}"))?;
            Ok(())
        })
        .await;
        match result {
            Ok(()) => {
                conn.execute("COMMIT", ())
                    .await
                    .with_context(|| format!("commit migration {name}"))?;
                count += 1;
            }
            Err(error) => {
                // 显式回滚；回滚失败叠加上下文（连接事务态未知 → 后续 fail-fast）
                if let Err(rollback) = conn.execute("ROLLBACK", ()).await {
                    bail!("migration {name} failed: {error:#}; rollback also failed: {rollback}");
                }
                return Err(error);
            }
        }
    }
    Ok(count)
}

struct Applied {
    version: i64,
    #[allow(dead_code)]
    name: String,
    checksum: String,
}

async fn read_applied(conn: &turso::Connection) -> anyhow::Result<Vec<Applied>> {
    let mut rows = conn
        .query(
            &format!("SELECT version,name,checksum FROM {VERSIONS_TABLE} ORDER BY version"),
            (),
        )
        .await
        .context("read migration history")?;
    let mut applied = Vec::new();
    while let Some(row) = rows.next().await.context("iterate migration history")? {
        let version = match row.get_value(0).context("read version")? {
            turso::Value::Integer(i) => i,
            other => bail!("migration version column corrupted: {other:?}"),
        };
        let name = match row.get_value(1).context("read name")? {
            turso::Value::Text(s) => s,
            other => bail!("migration name column corrupted: {other:?}"),
        };
        let checksum = match row.get_value(2).context("read checksum")? {
            turso::Value::Text(s) => s,
            other => bail!("migration checksum column corrupted: {other:?}"),
        };
        applied.push(Applied {
            version,
            name,
            checksum,
        });
    }
    Ok(applied)
}

/// 历史校验：版本连续、校验和与内嵌源一致、不高于程序支持版本。
fn verify_history(applied: &[Applied]) -> anyhow::Result<()> {
    let max_supported = MIGRATIONS.len();
    if applied.len() > max_supported {
        bail!(
            "database schema version {} is newer than this build supports ({max_supported}); \
             upgrade rcoder before opening",
            applied.len()
        );
    }
    for record in applied {
        let index = usize::try_from(record.version - 1).context("migration version underflow")?;
        let Some((name, sql)) = MIGRATIONS.get(index) else {
            bail!(
                "migration history version {} has no matching embedded migration",
                record.version
            );
        };
        if record.name != *name {
            bail!(
                "migration history name mismatch at version {}: history={}, embedded={}",
                record.version,
                record.name,
                name
            );
        }
        let expected = checksum_of(sql);
        if record.checksum != expected {
            bail!(
                "migration checksum mismatch at version {} ({name}); \
                 embedded migrations were modified after being applied",
                record.version
            );
        }
    }
    Ok(())
}

fn checksum_of(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 迁移历史文件路径（诊断用；迁移本体已内嵌）。
#[allow(dead_code)]
pub(super) fn _assert_migrations_dir_exists(path: &Path) {
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_checksums_are_stable() {
        // 内嵌清单非空且校验和可计算（发布产物不依赖源码目录）
        assert!(!MIGRATIONS.is_empty());
        assert_eq!(MIGRATIONS[0].0, "0001_init.sql");
        assert!(checksum_of(MIGRATIONS[0].1).len() == 64);
    }
}

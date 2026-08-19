//! userapp_metadata 表的数据访问。

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use shared_types::AppMetadataRecord;

/// upsert 元数据行（create_app/update_app 成功后调用）。
/// ON CONFLICT 不更新 created_at——业务首次创建时间不可变（同 app_id 重建不刷新）。
pub(in crate::pg) async fn upsert<'e>(
    db: impl PgExecutor<'e>,
    record: &AppMetadataRecord,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO userapp_metadata (app_id, name, tenant_id, space_id, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,now())
           ON CONFLICT (app_id) DO UPDATE SET
             name=EXCLUDED.name,
             tenant_id=EXCLUDED.tenant_id,
             space_id=EXCLUDED.space_id,
             updated_at=now()"#,
    )
    .bind(&record.app_id)
    .bind(&record.name)
    .bind(&record.tenant_id)
    .bind(&record.space_id)
    .bind(record.created_at)
    .execute(db)
    .await?;
    Ok(())
}

/// 行元组别名（fetch_all 的 query_as 目标;拆解见函数体）
type MetadataRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
);

/// 全量加载（启动恢复；空表返回空）
pub(in crate::pg) async fn fetch_all<'e>(
    db: impl PgExecutor<'e>,
) -> Result<Vec<AppMetadataRecord>, sqlx::Error> {
    let rows: Vec<MetadataRow> = sqlx::query_as(
        "SELECT app_id, name, tenant_id, space_id, created_at FROM userapp_metadata",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(app_id, name, tenant_id, space_id, created_at)| AppMetadataRecord {
                app_id,
                name,
                tenant_id,
                space_id,
                created_at,
            },
        )
        .collect())
}

/// 删除单行（storage/destroy 后调用）
pub(in crate::pg) async fn delete<'e>(
    db: impl PgExecutor<'e>,
    app_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM userapp_metadata WHERE app_id = $1")
        .bind(app_id)
        .execute(db)
        .await?;
    Ok(())
}

//! Activity is a lifecycle-bound observation, never a persisted wake policy.
use super::{ToastyUserAppStore, repo};
use crate::{db::models, userapp_lifecycle::storage};
use shared_types::{ActivityPersistence, ActivityRow, UserAppLifecycleState};

#[async_trait::async_trait]
impl ActivityPersistence for ToastyUserAppStore {
    async fn flush_batch(&self, mut rows: Vec<ActivityRow>) -> anyhow::Result<()> {
        // Match lifecycle writers' root-row lock order across a multi-app batch.
        rows.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        self.run(false, move |tx, backend| Box::pin(async move {
            for row in rows {
                let Some(at) = row.last_accessed else { continue; };
                repo::claim_app(tx, backend, &row.app_id).await?;
                let Some(app) = repo::app(tx, &row.app_id).await? else { continue; };
                if app.lifecycle_id != row.lifecycle_id || app.lifecycle_epoch != row.lifecycle_epoch || app.state != UserAppLifecycleState::Active { continue; }
                toasty::sql::statement(repo::sql(backend,
                    "INSERT INTO userapp_activity(app_id,lifecycle_id,scope,last_accessed_at_us,updated_at_us) VALUES($1,$2,'prod',$3,$4)
                     ON CONFLICT(app_id,lifecycle_id,scope) DO UPDATE SET last_accessed_at_us=EXCLUDED.last_accessed_at_us,updated_at_us=EXCLUDED.updated_at_us
                     WHERE userapp_activity.last_accessed_at_us<EXCLUDED.last_accessed_at_us"))
                    .bind(row.app_id).bind(row.lifecycle_id).bind(at.timestamp_micros())
                    .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
            }
            Ok(())
        })).await.map_err(anyhow::Error::new)
    }

    async fn load_all(&self) -> anyhow::Result<Vec<ActivityRow>> {
        self.run(true, |tx, _| {
            Box::pin(async move {
                let mut result = Vec::new();
                for row in models::Activity::all().exec(tx).await.map_err(storage)? {
                    if row.scope != "prod" {
                        continue;
                    }
                    let Some(app) = repo::app(tx, &row.app_id).await? else {
                        continue;
                    };
                    if app.lifecycle_id != row.lifecycle_id
                        || app.state != UserAppLifecycleState::Active
                    {
                        continue;
                    }
                    let at = chrono::DateTime::from_timestamp_micros(row.last_accessed_at_us)
                        .ok_or_else(|| storage(anyhow::anyhow!("Invalid activity timestamp")))?;
                    result.push(ActivityRow {
                        app_id: row.app_id,
                        lifecycle_id: row.lifecycle_id,
                        lifecycle_epoch: app.lifecycle_epoch,
                        last_accessed: Some(at),
                    });
                }
                Ok(result)
            })
        })
        .await
        .map_err(anyhow::Error::new)
    }

    async fn delete(&self, app_id: &str, lifecycle_id: &str) -> anyhow::Result<()> {
        let app_id = app_id.to_owned();
        let lifecycle_id = lifecycle_id.to_owned();
        self.run(false, move |tx, backend| Box::pin(async move {
            repo::claim_app(tx, backend, &app_id).await?;
            toasty::sql::statement(repo::sql(backend, "DELETE FROM userapp_activity WHERE app_id=$1 AND lifecycle_id=$2 AND scope='prod'"))
                .bind(app_id).bind(lifecycle_id).exec(tx).await.map_err(storage)?;
            Ok(())
        })).await.map_err(anyhow::Error::new)
    }
}

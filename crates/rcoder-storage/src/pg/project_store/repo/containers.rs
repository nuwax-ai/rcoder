//! Container registration writes. The transaction holds the container-name
//! advisory lock before reading; generations never follow wall-clock ordering.
use anyhow::{Result, ensure};
use shared_types::persistence::PersistenceOperationOutcome;
use toasty::Executor;
use toasty_core::schema::db::Type;

use super::store_repo::outcome;
use crate::{db::models::Container, pg::project_store::persist_ops::ContainerSnapshot};

pub(in crate::pg) async fn upsert_container(
    tx: &mut dyn Executor,
    c: &ContainerSnapshot,
) -> Result<PersistenceOperationOutcome> {
    ensure!(
        c.expected_revision >= 0 && !c.generation.is_empty(),
        "Invalid container write identity"
    );
    let revision = c
        .expected_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Container revision exhausted"))?;
    ensure!(
        c.predecessor.as_deref() != Some(c.generation.as_str()),
        "Container cannot replace its own generation"
    );
    ensure!(
        c.predecessor.is_some() == c.predecessor_revision.is_some()
            && c.predecessor_revision.is_none_or(|revision| revision > 0),
        "Incomplete container predecessor identity"
    );
    let retired = toasty::sql::query("SELECT container_name FROM container_tombstones WHERE (container_name=$1 AND container_generation=$2) OR (physical_uid IS NOT NULL AND physical_uid=$3)")
        .bind(&c.container_name).bind(&c.generation).bind_typed(c.container_id.as_deref(), Type::Text).exec(tx).await?;
    if !retired.is_empty() {
        return Ok(PersistenceOperationOutcome::Superseded);
    }

    let current = Container::filter_by_container_name(&c.container_name)
        .first()
        .exec(tx)
        .await?;
    if let Some(current) = current {
        if current.container_generation == c.generation {
            if current.row_revision != c.expected_revision {
                return Ok(PersistenceOperationOutcome::Superseded);
            }
            if current.container_id.is_some() && current.container_id != c.container_id {
                return Ok(PersistenceOperationOutcome::Superseded);
            }
        } else {
            if c.expected_revision != 0
                || c.predecessor.as_deref() != Some(current.container_generation.as_str())
                || c.predecessor_revision != Some(current.row_revision)
            {
                return Ok(PersistenceOperationOutcome::Superseded);
            }
            // Retire the captured predecessor and detach its exact references.
            // Never cascade an old project's identity into the new container.
            toasty::sql::statement("INSERT INTO container_tombstones(container_name,container_generation,physical_uid,retired_at_us) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                .bind(&c.container_name).bind(&current.container_generation)
                .bind_typed(current.container_id.as_deref(), Type::Text)
                .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await?;
            toasty::sql::statement("UPDATE projects SET container_name=NULL,container_generation=NULL WHERE container_name=$1 AND container_generation=$2")
                .bind(&c.container_name).bind(&current.container_generation).exec(tx).await?;
            toasty::sql::statement("DELETE FROM containers WHERE container_name=$1 AND container_generation=$2 AND row_revision=$3")
                .bind(&c.container_name).bind(&current.container_generation).bind(current.row_revision).exec(tx).await?;
        }
    } else if c.expected_revision != 0 {
        return Ok(PersistenceOperationOutcome::Superseded);
    }

    let changed = toasty::sql::statement(
        "INSERT INTO containers(container_name,container_generation,container_id,logical_id,service_type,container_ip,internal_port,external_port,status,service_url,last_activity_at_us,created_at_us,row_revision)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
         ON CONFLICT(container_name) DO UPDATE SET container_id=EXCLUDED.container_id,
           logical_id=EXCLUDED.logical_id,service_type=EXCLUDED.service_type,container_ip=EXCLUDED.container_ip,
           internal_port=EXCLUDED.internal_port,external_port=EXCLUDED.external_port,status=EXCLUDED.status,
           service_url=EXCLUDED.service_url,last_activity_at_us=GREATEST(containers.last_activity_at_us,EXCLUDED.last_activity_at_us),
           row_revision=EXCLUDED.row_revision
         WHERE containers.container_generation=EXCLUDED.container_generation AND containers.row_revision=$14
           AND (containers.container_id IS NULL OR containers.container_id=EXCLUDED.container_id)"
    ).bind(&c.container_name).bind(&c.generation).bind_typed(c.container_id.as_deref(), Type::Text)
        .bind(&c.logical_id).bind(&c.service_type).bind(&c.container_ip)
        .bind(i64::from(c.internal_port)).bind(i64::from(c.external_port)).bind(&c.status).bind(&c.service_url)
        .bind(c.last_activity.timestamp_micros()).bind(c.created_at.timestamp_micros()).bind(revision)
        .bind(c.expected_revision).exec(tx).await?;
    Ok(outcome(changed))
}

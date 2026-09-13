//! Compile-time specialization keeps transaction orchestration identical while
//! each backend supplies its own write-lock SQL and migrations. No Any driver.
macro_rules! implement_store {
    ($store:ident, $begin:literal, $locked:literal) => {
        #[async_trait::async_trait]
        impl shared_types::UserAppLifecycleStore for $store {
            async fn ensure_identity(
                &self,
                app_id: &str,
                user_id: &str,
            ) -> Result<UserAppLifecycleRecord, Error> {
                let proposed = domain::identity(app_id, user_id)?;
                let mut tx = self
                    .pool
                    .begin_with($begin)
                    .await
                    .map_err(storage)?;
                sqlx::query("INSERT INTO userapp_lifecycles(app_id,record) VALUES($1,$2) ON CONFLICT(app_id) DO NOTHING")
                    .bind(app_id).bind(serde_json::to_string(&proposed).map_err(storage)?)
                    .execute(&mut *tx).await.map_err(storage)?;
                let encoded: String = sqlx::query_scalar($locked)
                    .bind(app_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(storage)?;
                let app = serde_json::from_str(&encoded).map_err(storage)?;
                domain::validate_active(&app, user_id)?;
                tx.commit().await.map_err(storage)?;
                Ok(app)
            }

            async fn get_application(&self, app_id: &str) -> Result<Option<UserAppLifecycleRecord>, Error> {
                let encoded: Option<String> =
                    sqlx::query_scalar("SELECT record FROM userapp_lifecycles WHERE app_id=$1")
                        .bind(app_id)
                        .fetch_optional(&self.pool)
                        .await
                        .map_err(storage)?;
                encoded
                    .map(|value| serde_json::from_str(&value).map_err(storage))
                    .transpose()
            }

            async fn patch_metadata(
                &self,
                patch: &shared_types::UserAppMetadataPatch,
            ) -> Result<UserAppLifecycleRecord, Error> {
                let mut tx = self
                    .pool
                    .begin_with($begin)
                    .await
                    .map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked)
                    .bind(&patch.app_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?;
                let mut app: UserAppLifecycleRecord =
                    serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                domain::validate_active(&app, &patch.user_id)?;
                if app.lifecycle_id != patch.lifecycle_id {
                    return Err(Error::LifecycleConflict);
                }
                if app.metadata_revision != patch.expected_revision {
                    return Err(Error::VersionConflict);
                }
                let previous = app.clone();
                if let Some(name) = &patch.name {
                    app.name = name.clone();
                }
                if let Some(tenant) = &patch.tenant_id {
                    app.tenant_id = tenant.clone();
                }
                if let Some(space) = &patch.space_id {
                    app.space_id = space.clone();
                }
                if previous != app {
                    app.metadata_revision = app
                        .metadata_revision
                        .checked_add(1)
                        .ok_or_else(|| Error::InvalidOperation("metadata revision exhausted".into()))?;
                    let updated = sqlx::query("UPDATE userapp_lifecycles SET record=$2 WHERE app_id=$1")
                        .bind(&app.app_id)
                        .bind(serde_json::to_string(&app).map_err(storage)?)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage)?;
                    if updated.rows_affected() != 1 {
                        return Err(Error::VersionConflict);
                    }
                }
                tx.commit().await.map_err(storage)?;
                Ok(app)
            }

            async fn admit(
                &self,
                request: &shared_types::UserAppAdmission,
            ) -> Result<shared_types::UserAppAdmissionOutcome, Error> {
                let proposed = domain::identity(&request.app_id, &request.user_id)?;
                let mut tx = self
                    .pool
                    .begin_with($begin)
                    .await
                    .map_err(storage)?;
                sqlx::query("INSERT INTO userapp_lifecycles(app_id,record) VALUES($1,$2) ON CONFLICT(app_id) DO NOTHING")
                    .bind(&request.app_id).bind(serde_json::to_string(&proposed).map_err(storage)?)
                    .execute(&mut *tx).await.map_err(storage)?;
                let encoded: String = sqlx::query_scalar($locked)
                    .bind(&request.app_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(storage)?;
                let mut app: UserAppLifecycleRecord = serde_json::from_str(&encoded).map_err(storage)?;
                let duplicates: Vec<String> = sqlx::query_scalar("SELECT DISTINCT o.record FROM userapp_operations o LEFT JOIN userapp_operation_requests r ON r.operation_id=o.operation_id AND r.app_id=o.app_id WHERE o.app_id=$1 AND (o.operation_id=$2 OR r.request_id=$3)")
                    .bind(&request.app_id).bind(&request.operation_id).bind(&request.request_id)
                    .fetch_all(&mut *tx).await.map_err(storage)?;
                if duplicates.len() > 1 {
                    return Err(Error::InvalidOperation(
                        "request and operation identities refer to different operations".into(),
                    ));
                }
                let duplicate = duplicates
                    .first()
                    .map(|value| serde_json::from_str(value).map_err(storage))
                    .transpose()?;
                let active: Option<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2",
                )
                .bind(&request.app_id)
                .bind(&app.current_operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
                if app.current_operation_id.is_some() && active.is_none() {
                    return Err(Error::InvalidOperation(
                        "current operation record is missing".into(),
                    ));
                }
                let active = active
                    .map(|value| serde_json::from_str(&value).map_err(storage))
                    .transpose()?;
                let result = domain::admission(&mut app, request, duplicate, active)?;
                if let shared_types::UserAppAdmissionOutcome::Accepted(operation) = &result {
                    sqlx::query("INSERT INTO userapp_operations(operation_id,app_id,request_id,terminal,record) VALUES($1,$2,$3,0,$4)")
                        .bind(&operation.operation_id).bind(&operation.app_id).bind(&operation.request_id)
                        .bind(serde_json::to_string(operation).map_err(storage)?)
                        .execute(&mut *tx).await.map_err(storage)?;
                    let updated = sqlx::query("UPDATE userapp_lifecycles SET record=$2 WHERE app_id=$1")
                        .bind(&app.app_id)
                        .bind(serde_json::to_string(&app).map_err(storage)?)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage)?;
                    if updated.rows_affected() != 1 {
                        return Err(Error::VersionConflict);
                    }
                }
                // Joining is also an accepted idempotent request. Register its
                // alias in the same transaction as admission, even if no runtime
                // work was added. Keep the original caller token on the operation.
                if let Some(request_id) = &request.request_id {
                    let operation = match &result {
                        shared_types::UserAppAdmissionOutcome::Accepted(op)
                        | shared_types::UserAppAdmissionOutcome::Existing(op) => op,
                    };
                    let inserted = sqlx::query("INSERT INTO userapp_operation_requests(app_id,request_id,operation_id) VALUES($1,$2,$3) ON CONFLICT(app_id,request_id) DO NOTHING")
                        .bind(&request.app_id).bind(request_id).bind(&operation.operation_id)
                        .execute(&mut *tx).await.map_err(storage)?;
                    if inserted.rows_affected() == 0 {
                        let owner: String = sqlx::query_scalar("SELECT operation_id FROM userapp_operation_requests WHERE app_id=$1 AND request_id=$2")
                            .bind(&request.app_id).bind(request_id).fetch_one(&mut *tx).await.map_err(storage)?;
                        if owner != operation.operation_id {
                            return Err(Error::VersionConflict);
                        }
                    }
                }
                tx.commit().await.map_err(storage)?;
                Ok(result)
            }

            async fn advance(
                &self,
                progress: &shared_types::UserAppOperationProgress,
            ) -> Result<UserAppOperationRecord, Error> {
                let mut tx = self
                    .pool
                    .begin_with($begin)
                    .await
                    .map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked)
                    .bind(&progress.app_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?;
                let mut app = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2",
                )
                .bind(&progress.app_id)
                .bind(&progress.operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
                let mut operation =
                    serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                domain::advance(&mut app, &mut operation, progress)?;
                let updated = sqlx::query(
                    "UPDATE userapp_operations SET record=$2,terminal=$3 WHERE operation_id=$1",
                )
                .bind(&operation.operation_id)
                .bind(serde_json::to_string(&operation).map_err(storage)?)
                .bind(i32::from(operation.state.is_terminal()))
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
                if updated.rows_affected() != 1 {
                    return Err(Error::VersionConflict);
                }
                let updated = sqlx::query("UPDATE userapp_lifecycles SET record=$2 WHERE app_id=$1")
                    .bind(&app.app_id)
                    .bind(serde_json::to_string(&app).map_err(storage)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                if updated.rows_affected() != 1 {
                    return Err(Error::VersionConflict);
                }
                tx.commit().await.map_err(storage)?;
                Ok(operation)
            }

            async fn get_operation(
                &self,
                app_id: &str,
                operation_id: &str,
            ) -> Result<Option<UserAppOperationRecord>, Error> {
                let encoded: Option<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2",
                )
                .bind(app_id)
                .bind(operation_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(storage)?;
                encoded
                    .map(|value| serde_json::from_str(&value).map_err(storage))
                    .transpose()
            }

            async fn unfinished_operations(
                &self,
                limit: u32,
            ) -> Result<Vec<UserAppOperationRecord>, Error> {
                if limit == 0 {
                    return Err(Error::InvalidOperation(
                        "scan limit must be positive".into(),
                    ));
                }
                let rows: Vec<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_operations WHERE terminal=0 ORDER BY operation_id LIMIT $1",
                )
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
                rows.iter()
                    .map(|value| serde_json::from_str(value).map_err(storage))
                    .collect()
            }

            async fn recreate(
                &self,
                app_id: &str,
                user_id: &str,
                expected_lifecycle_id: &str,
                request_id: &str,
            ) -> Result<UserAppLifecycleRecord, Error> {
                if request_id.is_empty() {
                    return Err(Error::InvalidOperation(
                        "recreation requires request_id".into(),
                    ));
                }
                let mut tx = self
                    .pool
                    .begin_with($begin)
                    .await
                    .map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked)
                    .bind(app_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?;
                let mut app: UserAppLifecycleRecord =
                    serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                domain::validate_owner(&app, user_id)?;
                let previous: Option<(String,String)> = sqlx::query_as("SELECT previous_lifecycle_id,new_lifecycle_id FROM userapp_recreations WHERE app_id=$1 AND request_id=$2")
                    .bind(app_id).bind(request_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                if let Some((old, new)) = previous {
                    if old != expected_lifecycle_id || new != app.lifecycle_id {
                        return Err(Error::LifecycleConflict);
                    }
                    tx.commit().await.map_err(storage)?;
                    return Ok(app);
                }
                if app.lifecycle_id != expected_lifecycle_id
                    || app.state != shared_types::UserAppLifecycleState::Deleted
                    || app.current_operation_id.is_some()
                {
                    return Err(Error::LifecycleConflict);
                }
                app.lifecycle_id = uuid::Uuid::new_v4().to_string();
                app.lifecycle_epoch = app
                    .lifecycle_epoch
                    .checked_add(1)
                    .ok_or_else(|| Error::InvalidOperation("lifecycle epoch exhausted".into()))?;
                app.metadata_revision = 1;
                app.state = shared_types::UserAppLifecycleState::Active;
                app.created_at = chrono::Utc::now();
                app.name = None;
                app.tenant_id = None;
                app.space_id = None;
                sqlx::query("INSERT INTO userapp_recreations(app_id,request_id,previous_lifecycle_id,new_lifecycle_id) VALUES($1,$2,$3,$4)")
                    .bind(app_id).bind(request_id).bind(expected_lifecycle_id).bind(&app.lifecycle_id)
                    .execute(&mut *tx).await.map_err(storage)?;
                let updated = sqlx::query("UPDATE userapp_lifecycles SET record=$2 WHERE app_id=$1")
                    .bind(app_id)
                    .bind(serde_json::to_string(&app).map_err(storage)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                if updated.rows_affected() != 1 {
                    return Err(Error::VersionConflict);
                }
                tx.commit().await.map_err(storage)?;
                Ok(app)
            }
        }
    };
}
pub(super) use implement_store;

//! Compile-time specialization keeps transaction orchestration identical while
//! each backend supplies its own write-lock SQL and migrations. No Any driver.
macro_rules! implement_store {
    ($store:ident, $begin:literal, $locked:literal, $control_snapshot:literal) => {
        #[async_trait::async_trait]
        impl shared_types::UserAppLifecycleStore for $store {
            async fn get_resource_binding(&self, service_type: &shared_types::ServiceType, physical_uid: &str) -> Result<Option<shared_types::UserAppResourceBinding>, Error> {
                let record: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_resource_bindings WHERE service_type=$1 AND physical_uid=$2")
                    .bind(service_type.to_string()).bind(physical_uid).fetch_optional(&self.pool).await.map_err(storage)?;
                record.map(|record| serde_json::from_str(&record).map_err(storage)).transpose()
            }

            async fn commit_resource_binding(&self, binding: &shared_types::UserAppResourceBinding, progress: &shared_types::UserAppOperationProgress) -> Result<UserAppOperationRecord, Error> {
                if progress.app_id != binding.app_id || progress.lifecycle_id != binding.lifecycle_id || progress.state != shared_types::UserAppOperationState::Succeeded || binding.adopted_by_operation != progress.operation_id {
                    return Err(Error::InvalidOperation("Binding requires its successful adoption operation".into()));
                }
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked).bind(&binding.app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let mut app: UserAppLifecycleRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                domain::validate_active(&app)?;
                if app.lifecycle_id != binding.lifecycle_id { return Err(Error::LifecycleConflict); }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2")
                    .bind(&binding.app_id).bind(&progress.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let mut operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if operation.kind != shared_types::UserAppOperationKind::AdoptBuilder { return Err(Error::InvalidOperation("Binding requires explicit adoption admission".into())); }
                let context = shared_types::UserAppExecutionContext { app_id: app.app_id.clone(), lifecycle_id: app.lifecycle_id.clone(), operation_id: operation.operation_id.clone(), executor_id: progress.executor_id.clone(), request_fingerprint: operation.request_fingerprint.clone() };
                binding.validate(&context, &binding.physical_uid).map_err(Error::InvalidOperation)?;
                domain::advance(&mut app, &mut operation, progress)?;
                sqlx::query("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,record) VALUES($1,$2,$3,$4) ON CONFLICT(service_type,physical_uid) DO NOTHING")
                    .bind(binding.service_type.to_string()).bind(&binding.physical_uid).bind(&binding.app_id).bind(serde_json::to_string(binding).map_err(storage)?).execute(&mut *tx).await.map_err(storage)?;
                let encoded: String = sqlx::query_scalar("SELECT record FROM userapp_resource_bindings WHERE service_type=$1 AND physical_uid=$2")
                    .bind(binding.service_type.to_string()).bind(&binding.physical_uid).fetch_one(&mut *tx).await.map_err(storage)?;
                let previous: shared_types::UserAppResourceBinding = serde_json::from_str(&encoded).map_err(storage)?;
                previous.validate(&context, &binding.physical_uid).map_err(|_| Error::LifecycleConflict)?;
                let updated = sqlx::query("UPDATE userapp_operations SET record=$2,terminal=1 WHERE operation_id=$1")
                    .bind(&operation.operation_id).bind(serde_json::to_string(&operation).map_err(storage)?).execute(&mut *tx).await.map_err(storage)?;
                if updated.rows_affected()!=1 { return Err(Error::VersionConflict); }
                let updated = sqlx::query("UPDATE userapp_lifecycles SET record=$2 WHERE app_id=$1")
                    .bind(&app.app_id).bind(serde_json::to_string(&app).map_err(storage)?).execute(&mut *tx).await.map_err(storage)?;
                if updated.rows_affected()!=1 { return Err(Error::VersionConflict); }
                sqlx::query("DELETE FROM userapp_operation_inputs WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3")
                    .bind(&operation.operation_id).bind(&operation.app_id).bind(&operation.lifecycle_id).execute(&mut *tx).await.map_err(storage)?;
                tx.commit().await.map_err(storage)?;
                Ok(operation)
            }

            async fn list_control_snapshots(&self, after_app_id: Option<&str>, limit: u32) -> Result<Vec<shared_types::UserAppControlSnapshot>, Error> {
                if limit == 0 {
                    return Err(Error::InvalidOperation("page limit must be positive".into()));
                }
                let rows: Vec<(String, Option<String>)> = sqlx::query_as($control_snapshot)
                    .bind(after_app_id.unwrap_or(""))
                    .bind(i64::from(limit))
                    .fetch_all(&self.pool).await.map_err(storage)?;
                rows.into_iter().map(|(application, operation)| {
                    let application: UserAppLifecycleRecord = serde_json::from_str(&application).map_err(storage)?;
                    let operation: Option<UserAppOperationRecord> = operation.map(|encoded| serde_json::from_str(&encoded).map_err(storage)).transpose()?;
                    match (application.current_operation_id.as_deref(), operation.as_ref()) {
                        (None, None) => {},
                        (Some(id), Some(operation)) if operation.operation_id == id
                            && operation.app_id == application.app_id
                            && operation.lifecycle_id == application.lifecycle_id
                            && !operation.state.is_terminal() => {},
                        _ => return Err(Error::InvalidOperation("Lifecycle current-operation linkage is inconsistent".into())),
                    }
                    Ok(shared_types::UserAppControlSnapshot { application, operation })
                }).collect()
            }

            async fn ensure_identity(
                &self,
                app_id: &str,
            ) -> Result<UserAppLifecycleRecord, Error> {
                let proposed = domain::identity(app_id)?;
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
                domain::validate_active(&app)?;
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

            async fn list_applications(
                &self,
                after_app_id: Option<&str>,
                limit: u32,
            ) -> Result<Vec<UserAppLifecycleRecord>, Error> {
                if limit == 0 {
                    return Err(Error::InvalidOperation("page limit must be positive".into()));
                }
                let rows: Vec<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_lifecycles WHERE app_id > $1 ORDER BY app_id LIMIT $2",
                )
                .bind(after_app_id.unwrap_or(""))
                .bind(i64::from(limit))
                .fetch_all(&self.pool).await.map_err(storage)?;
                rows.iter().map(|row| serde_json::from_str(row).map_err(storage)).collect()
            }

            async fn import_application(
                &self,
                legacy: &shared_types::AppMetadataRecord,
            ) -> Result<UserAppLifecycleRecord, Error> {
                // Owner validation removed with user binding removal
                let mut proposed = domain::identity(&legacy.app_id)?;
                proposed.name = legacy.name.clone();
                proposed.tenant_id = legacy.tenant_id.clone();
                proposed.space_id = legacy.space_id.clone();
                proposed.created_at = legacy.created_at;
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                sqlx::query("INSERT INTO userapp_lifecycles(app_id,record) VALUES($1,$2) ON CONFLICT(app_id) DO NOTHING")
                    .bind(&legacy.app_id).bind(serde_json::to_string(&proposed).map_err(storage)?)
                    .execute(&mut *tx).await.map_err(storage)?;
                let encoded: String = sqlx::query_scalar($locked)
                    .bind(&legacy.app_id).fetch_one(&mut *tx).await.map_err(storage)?;
                let app = serde_json::from_str(&encoded).map_err(storage)?;
                tx.commit().await.map_err(storage)?;
                Ok(app)
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
                let previous = app.clone();
                domain::patch_metadata(&mut app, patch)?;
                if previous != app {
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
                self.admit_with_input(request, None).await
            }

            async fn admit_with_input(
                &self,
                request: &shared_types::UserAppAdmission,
                input: Option<&shared_types::UserAppExecutionInput>,
            ) -> Result<shared_types::UserAppAdmissionOutcome, Error> {
                match (&request.command, input) {
                    (None, Some(input)) if request.kind == shared_types::UserAppOperationKind::AdoptBuilder && request.request_fingerprint == input.digest() => {},
                    (None, _) if request.kind == shared_types::UserAppOperationKind::AdoptBuilder => return Err(Error::InvalidOperation("Adoption requires its original input digest".into())),
                    (Some(shared_types::UserAppControlCommand::Create { input_digest } | shared_types::UserAppControlCommand::Update { input_digest } | shared_types::UserAppControlCommand::Deploy { input_digest, .. }), Some(input)) if *input_digest == input.digest() => {},
                    (Some(shared_types::UserAppControlCommand::Create { .. } | shared_types::UserAppControlCommand::Update { .. } | shared_types::UserAppControlCommand::Deploy { .. }), _) | (_, Some(_)) => return Err(Error::InvalidOperation("Private execution input does not match command digest".into())),
                    (_, None) => {},
                }
                let proposed = domain::identity(&request.app_id)?;
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
                if let Some(request_id) = &request.request_id {
                    let recreation: Option<String> = sqlx::query_scalar("SELECT request_id FROM userapp_recreations WHERE app_id=$1 AND request_id=$2")
                        .bind(&request.app_id).bind(request_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if recreation.is_some() {
                        return Err(Error::InvalidOperation("request identity was already used for lifecycle recreation".into()));
                    }
                }
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
                    if let Some(input) = input {
                        sqlx::query("INSERT INTO userapp_operation_inputs(operation_id,app_id,lifecycle_id,payload) VALUES($1,$2,$3,$4)")
                            .bind(&operation.operation_id).bind(&operation.app_id).bind(&operation.lifecycle_id).bind(input.encoded())
                            .execute(&mut *tx).await.map_err(storage)?;
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

            async fn read_execution_input(
                &self,
                context: &shared_types::UserAppExecutionContext,
            ) -> Result<shared_types::UserAppExecutionInput, Error> {
                context.validate_identity(&context.app_id).map_err(Error::InvalidOperation)?;
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked).bind(&context.app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let app: UserAppLifecycleRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if app.lifecycle_id != context.lifecycle_id || app.current_operation_id.as_deref() != Some(context.operation_id.as_str()) {
                    return Err(Error::LifecycleConflict);
                }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2")
                    .bind(&context.app_id).bind(&context.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if operation.executor_id.as_deref() != Some(context.executor_id.as_str())
                    || operation.lifecycle_id != context.lifecycle_id || operation.request_fingerprint != context.request_fingerprint
                    || operation.state != shared_types::UserAppOperationState::Running {
                    return Err(Error::VersionConflict);
                }
                let payload: Option<String> = sqlx::query_scalar("SELECT payload FROM userapp_operation_inputs WHERE app_id=$1 AND operation_id=$2 AND lifecycle_id=$3")
                    .bind(&context.app_id).bind(&context.operation_id).bind(&context.lifecycle_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let input = shared_types::UserAppExecutionInput::new(payload.ok_or(Error::NotFound)?);
                match &operation.command {
                    None if operation.kind == shared_types::UserAppOperationKind::AdoptBuilder && operation.request_fingerprint == input.digest() => {},
                    Some(shared_types::UserAppControlCommand::Create { input_digest } | shared_types::UserAppControlCommand::Update { input_digest } | shared_types::UserAppControlCommand::Deploy { input_digest, .. }) if *input_digest == input.digest() => {},
                    _ => return Err(Error::InvalidOperation("Stored execution input digest mismatch".into())),
                }
                tx.commit().await.map_err(storage)?;
                Ok(input)
            }

            async fn bind_operation_deadline(&self, app_id: &str, operation_id: &str, lifecycle_id: &str, deadline_epoch_ms: i64) -> Result<i64, Error> {
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked).bind(app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let app: UserAppLifecycleRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if app.lifecycle_id != lifecycle_id || app.current_operation_id.as_deref() != Some(operation_id) { return Err(Error::LifecycleConflict); }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2").bind(app_id).bind(operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if operation.lifecycle_id != lifecycle_id || operation.state.is_terminal() { return Err(Error::VersionConflict); }
                // bind-once：INSERT ON CONFLICT DO NOTHING；已存在 → 返回持久值
                // （不同副本配置不同也不得各绑各的候选 deadline）
                sqlx::query("INSERT INTO userapp_operation_deadlines(operation_id,app_id,lifecycle_id,deadline_ms) VALUES($1,$2,$3,$4) ON CONFLICT(operation_id) DO NOTHING")
                    .bind(operation_id).bind(app_id).bind(lifecycle_id).bind(deadline_epoch_ms).execute(&mut *tx).await.map_err(storage)?;
                let persisted: i64 = sqlx::query_scalar("SELECT deadline_ms FROM userapp_operation_deadlines WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3")
                    .bind(operation_id).bind(app_id).bind(lifecycle_id).fetch_one(&mut *tx).await.map_err(storage)?;
                tx.commit().await.map_err(storage)?;
                Ok(persisted)
            }

            async fn operation_deadline(&self, app_id: &str, operation_id: &str) -> Result<Option<i64>, Error> {
                let row: Option<i64> = sqlx::query_scalar("SELECT deadline_ms FROM userapp_operation_deadlines WHERE app_id=$1 AND operation_id=$2")
                    .bind(app_id).bind(operation_id).fetch_optional(&self.pool).await.map_err(storage)?;
                Ok(row)
            }

            async fn bind_operation_lease(&self, context: &shared_types::UserAppExecutionContext, receipt: &shared_types::UserAppOperationLeaseReceipt) -> Result<(), Error> {
                context.validate_identity(&context.app_id).map_err(Error::InvalidOperation)?;
                receipt.validate().map_err(Error::InvalidOperation)?;
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked).bind(&context.app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let app: UserAppLifecycleRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if app.lifecycle_id != context.lifecycle_id || app.current_operation_id.as_deref() != Some(context.operation_id.as_str()) { return Err(Error::LifecycleConflict); }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2").bind(&context.app_id).bind(&context.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                let builder = matches!(operation.kind, shared_types::UserAppOperationKind::EnsureBuilder | shared_types::UserAppOperationKind::StopBuilder | shared_types::UserAppOperationKind::RestartBuilder | shared_types::UserAppOperationKind::AdoptBuilder);
                if operation.state != shared_types::UserAppOperationState::Running || operation.executor_id.as_deref() != Some(context.executor_id.as_str()) || operation.lifecycle_id != context.lifecycle_id || operation.request_fingerprint != context.request_fingerprint
                    || builder != (*receipt.service_type() == shared_types::ServiceType::UserappBuilder) { return Err(Error::VersionConflict); }
                let binding = shared_types::UserAppOperationLeaseBinding { context: context.clone(), receipt: receipt.clone() };
                let encoded = serde_json::to_string(&binding).map_err(storage)?;
                let inserted = sqlx::query("INSERT INTO userapp_operation_leases(operation_id,app_id,lifecycle_id,record) VALUES($1,$2,$3,$4) ON CONFLICT(operation_id) DO NOTHING")
                    .bind(&context.operation_id).bind(&context.app_id).bind(&context.lifecycle_id).bind(&encoded).execute(&mut *tx).await.map_err(storage)?;
                if inserted.rows_affected() == 0 {
                    let current: String = sqlx::query_scalar("SELECT record FROM userapp_operation_leases WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3")
                        .bind(&context.operation_id).bind(&context.app_id).bind(&context.lifecycle_id).fetch_one(&mut *tx).await.map_err(storage)?;
                    let current: shared_types::UserAppOperationLeaseBinding = serde_json::from_str(&current).map_err(storage)?;
                    if current != binding { return Err(Error::VersionConflict); }
                }
                tx.commit().await.map_err(storage)?;
                Ok(())
            }

            async fn get_operation_lease(&self, app_id: &str, operation_id: &str) -> Result<Option<shared_types::UserAppOperationLeaseBinding>, Error> {
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operation_leases WHERE app_id=$1 AND operation_id=$2").bind(app_id).bind(operation_id).fetch_optional(&self.pool).await.map_err(storage)?;
                encoded.map(|value| serde_json::from_str(&value).map_err(storage)).transpose()
            }

            async fn terminal_operation_leases(&self, after: Option<&str>, limit: u32) -> Result<Vec<shared_types::UserAppOperationLeaseBinding>, Error> {
                use sqlx::Row as _;
                let rows = sqlx::query("SELECT l.record AS lease_record,o.record AS operation_record FROM userapp_operation_leases l JOIN userapp_operations o ON l.operation_id=o.operation_id WHERE o.terminal=1 AND l.operation_id > $1 ORDER BY l.operation_id LIMIT $2")
                    .bind(after.unwrap_or("")).bind(i64::from(limit.clamp(1, 1000))).fetch_all(&self.pool).await.map_err(storage)?;
                let mut bindings = Vec::new();
                for row in rows {
                    let operation: UserAppOperationRecord = serde_json::from_str(row.try_get::<&str,_>("operation_record").map_err(storage)?).map_err(storage)?;
                    if operation.state.is_terminal() {
                        bindings.push(serde_json::from_str(row.try_get::<&str,_>("lease_record").map_err(storage)?).map_err(storage)?);
                    }
                }
                Ok(bindings)
            }

            async fn forget_operation_lease(&self, binding: &shared_types::UserAppOperationLeaseBinding) -> Result<(), Error> {
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                // Lock the application row even for an old terminal lifecycle.
                let _locked_application: Option<String> = sqlx::query_scalar($locked).bind(&binding.context.app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2").bind(&binding.context.app_id).bind(&binding.context.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if !operation.state.is_terminal() || operation.lifecycle_id != binding.context.lifecycle_id || operation.executor_id.as_deref() != Some(binding.context.executor_id.as_str()) || operation.request_fingerprint != binding.context.request_fingerprint { return Err(Error::VersionConflict); }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operation_leases WHERE app_id=$1 AND operation_id=$2").bind(&binding.context.app_id).bind(&binding.context.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                if let Some(encoded) = encoded {
                    let stored: shared_types::UserAppOperationLeaseBinding = serde_json::from_str(&encoded).map_err(storage)?;
                    if &stored != binding { return Err(Error::VersionConflict); }
                    let deleted = sqlx::query("DELETE FROM userapp_operation_leases WHERE operation_id=$1 AND app_id=$2").bind(&binding.context.operation_id).bind(&binding.context.app_id).execute(&mut *tx).await.map_err(storage)?;
                    if deleted.rows_affected() != 1 { return Err(Error::VersionConflict); }
                }
                tx.commit().await.map_err(storage)?;
                Ok(())
            }

            async fn reserve_completed_operation(&self, snapshot: &UserAppOperationRecord) -> Result<UserAppOperationRecord, Error> {
                let mut tx = self.pool.begin_with($begin).await.map_err(storage)?;
                let encoded: Option<String> = sqlx::query_scalar($locked).bind(&snapshot.app_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let app: UserAppLifecycleRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if app.lifecycle_id != snapshot.lifecycle_id || app.current_operation_id.as_deref() != Some(snapshot.operation_id.as_str()) { return Err(Error::LifecycleConflict); }
                let encoded: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operations WHERE app_id=$1 AND operation_id=$2").bind(&snapshot.app_id).bind(&snapshot.operation_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let mut operation: UserAppOperationRecord = serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)?;
                if &operation != snapshot || !matches!(operation.state, shared_types::UserAppOperationState::Running | shared_types::UserAppOperationState::RecoveryRequired) || !shared_types::userapp_operation_has_final_evidence(&operation) { return Err(Error::VersionConflict); }
                // EnsureBuilder's typed final evidence follows successful runtime
                // completion, which has already released its creation mutex.
                if operation.kind == shared_types::UserAppOperationKind::EnsureBuilder {
                    let _evidence: shared_types::BuilderCreationEvidence = serde_json::from_value(operation.checkpoint.clone()).map_err(storage)?;
                    if app.state != shared_types::UserAppLifecycleState::Active { return Err(Error::LifecycleConflict); }
                } else {
                let binding: Option<String> = sqlx::query_scalar("SELECT record FROM userapp_operation_leases WHERE app_id=$1 AND operation_id=$2 AND lifecycle_id=$3").bind(&operation.app_id).bind(&operation.operation_id).bind(&operation.lifecycle_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                let binding: shared_types::UserAppOperationLeaseBinding = serde_json::from_str(&binding.ok_or(Error::NotFound)?).map_err(storage)?;
                if binding.context.app_id != operation.app_id || binding.context.operation_id != operation.operation_id || binding.context.lifecycle_id != operation.lifecycle_id || &binding.context.executor_id != operation.executor_id.as_ref().ok_or(Error::VersionConflict)? || binding.context.request_fingerprint != operation.request_fingerprint { return Err(Error::VersionConflict); }
                binding.receipt.validate().map_err(Error::InvalidOperation)?;
                }
                operation.revision = operation.revision.checked_add(1).ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
                operation.state = shared_types::UserAppOperationState::Running;
                let updated = sqlx::query("UPDATE userapp_operations SET record=$2 WHERE operation_id=$1 AND app_id=$3").bind(&operation.operation_id).bind(serde_json::to_string(&operation).map_err(storage)?).bind(&operation.app_id).execute(&mut *tx).await.map_err(storage)?;
                if updated.rows_affected() != 1 { return Err(Error::VersionConflict); }
                tx.commit().await.map_err(storage)?;
                Ok(operation)
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
                if operation.state.is_terminal() {
                    sqlx::query("DELETE FROM userapp_operation_inputs WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3")
                        .bind(&operation.operation_id).bind(&operation.app_id).bind(&operation.lifecycle_id)
                        .execute(&mut *tx).await.map_err(storage)?;
                }
                tx.commit().await.map_err(storage)?;
                Ok(operation)
            }

            async fn get_operation_by_request(
                &self,
                app_id: &str,
                request_id: &str,
            ) -> Result<Option<UserAppOperationRecord>, Error> {
                domain::validate_request_id(request_id)?;
                let encoded: Option<String> = sqlx::query_scalar("SELECT o.record FROM userapp_operation_requests r JOIN userapp_operations o ON o.app_id=r.app_id AND o.operation_id=r.operation_id WHERE r.app_id=$1 AND r.request_id=$2")
                    .bind(app_id).bind(request_id).fetch_optional(&self.pool).await.map_err(storage)?;
                encoded.map(|record| serde_json::from_str(&record).map_err(storage)).transpose()
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
                after_operation_id: Option<&str>,
                limit: u32,
            ) -> Result<Vec<UserAppOperationRecord>, Error> {
                if limit == 0 {
                    return Err(Error::InvalidOperation(
                        "scan limit must be positive".into(),
                    ));
                }
                let rows: Vec<String> = sqlx::query_scalar(
                    "SELECT record FROM userapp_operations WHERE terminal=0 AND operation_id > $1 ORDER BY operation_id LIMIT $2",
                )
                .bind(after_operation_id.unwrap_or(""))
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
                expected_lifecycle_id: &str,
                request_id: &str,
            ) -> Result<UserAppLifecycleRecord, Error> {
                domain::validate_request_id(request_id)?;
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
                let control: Option<String> = sqlx::query_scalar("SELECT request_id FROM userapp_operation_requests WHERE app_id=$1 AND request_id=$2")
                    .bind(app_id).bind(request_id).fetch_optional(&mut *tx).await.map_err(storage)?;
                if control.is_some() {
                    return Err(Error::InvalidOperation("request identity was already used for a control operation".into()));
                }
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
                app.runtime_policy = shared_types::UserAppRuntimePolicy::default();
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

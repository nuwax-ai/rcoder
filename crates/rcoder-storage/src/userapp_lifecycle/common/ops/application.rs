use super::*;

pub(crate) async fn ensure_identity(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    let app = repo::ensure(tx, backend, &domain::identity(app_id)?).await?;
    domain::validate_active(&app)?;
    Ok(app)
}
pub(crate) async fn get_application(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
) -> Result<Option<UserAppLifecycleRecord>, Error> {
    repo::app(tx, app_id).await
}
pub(crate) async fn list_applications(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppLifecycleRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "page limit must be positive".into(),
        ));
    }
    let ids = repo::strings(
        toasty::sql::query(repo::sql(
            backend,
            "SELECT app_id FROM userapps WHERE app_id>$1 ORDER BY app_id LIMIT $2",
        ))
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .exec(tx)
        .await
        .map_err(storage)?,
    )?;
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        records.push(repo::app(tx, &id).await?.ok_or(Error::NotFound)?);
    }
    Ok(records)
}
pub(crate) async fn list_control_snapshots(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppControlSnapshot>, Error> {
    let applications = list_applications(tx, backend, after, limit).await?;
    let mut snapshots = Vec::with_capacity(applications.len());
    for application in applications {
        let operations = repo::active(tx, &application).await?;
        snapshots.push(UserAppControlSnapshot {
            application,
            operations,
        });
    }
    Ok(snapshots)
}
pub(crate) async fn patch_metadata(
    tx: &mut dyn Executor,
    backend: Backend,
    patch: &UserAppMetadataPatch,
) -> Result<UserAppLifecycleRecord, Error> {
    repo::claim_app(tx, backend, &patch.app_id).await?;
    let mut app = repo::app(tx, &patch.app_id).await?.ok_or(Error::NotFound)?;
    let previous = app.clone();
    domain::patch_metadata(&mut app, patch)?;
    if app != previous {
        repo::save_app(
            tx,
            backend,
            &app,
            &previous.lifecycle_id,
            previous.metadata_revision,
        )
        .await?;
    }
    Ok(app)
}

pub(crate) async fn admit(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
) -> Result<UserAppAdmissionOutcome, Error> {
    admit_with_input(tx, backend, request, None).await
}
pub(crate) async fn admit_with_input(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
    input: Option<&UserAppExecutionInput>,
) -> Result<UserAppAdmissionOutcome, Error> {
    admit_with_configuration(tx, backend, request, input, None).await
}
pub(crate) async fn admit_with_configuration(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
    input: Option<&UserAppExecutionInput>,
    pg: Option<&StartPgCredential>,
) -> Result<UserAppAdmissionOutcome, Error> {
    if pg.is_some()
        && (!matches!(request.command, Some(UserAppControlCommand::Deploy { .. }))
            || input.is_none())
    {
        return Err(Error::InvalidOperation(
            "Inline credentials require a private deployment input".into(),
        ));
    }
    validate_input(
        request.kind,
        &request.request_fingerprint,
        request.command.as_ref(),
        input,
    )?;
    let mut app = repo::ensure(tx, backend, &domain::identity(&request.app_id)?).await?;
    let previous = app.clone();
    let by_id = repo::operation(tx, &request.app_id, &request.operation_id).await?;
    let mapping = if let Some(id) = &request.request_id {
        repo::request(tx, &request.app_id, id).await?
    } else {
        None
    };
    let by_request = if let Some(mapping) = &mapping {
        if mapping.target_kind == "recreate" {
            return Err(Error::InvalidOperation(
                "request identity was already used for lifecycle recreation".into(),
            ));
        }
        repo::request_operation(tx, &request.app_id, mapping).await?
    } else {
        None
    };
    if let (Some(a), Some(b)) = (&by_id, &by_request)
        && a.operation_id != b.operation_id
    {
        return Err(Error::InvalidOperation(
            "request and operation identities refer to different operations".into(),
        ));
    }
    let active = repo::active(tx, &app).await?;
    let mut result = domain::admission(&mut app, request, by_id.or(by_request), &active)?;
    if let UserAppAdmissionOutcome::Accepted(operation) = &mut result {
        crate::userapp_lifecycle::common::compute::guard_admission(tx, &app, operation.scope)
            .await?;
        configuration::guard_database_admin(tx, operation).await?;
        operation.created_at = operation.created_at.trunc_subsecs(6);
        repo::insert_operation(tx, operation).await?;
        #[cfg(test)]
        crate::userapp_lifecycle::common::transaction_fault_tests::check("admission_operation")?;
        if let Some(pg) = pg {
            configuration::seed_deployment_credentials(tx, backend, operation, pg).await?;
            configuration::capture(tx, operation).await?;
        }
        if let Some(input) = input {
            models::OperationInput::create()
                .operation_id(&operation.operation_id)
                .app_id(&operation.app_id)
                .lifecycle_id(&operation.lifecycle_id)
                .payload_version(1)
                .payload(input.encoded())
                .payload_digest(input.digest())
                .created_at_us(chrono::Utc::now().timestamp_micros())
                .exec(tx)
                .await
                .map_err(storage)?;
        }

        #[cfg(test)]
        crate::userapp_lifecycle::common::transaction_fault_tests::check("admission_input")?;
        repo::save_app(
            tx,
            backend,
            &app,
            &previous.lifecycle_id,
            previous.metadata_revision,
        )
        .await?;

        #[cfg(test)]
        crate::userapp_lifecycle::common::transaction_fault_tests::check("admission_application")?;
        repo::save_slots(tx, backend, &app, &previous).await?;
        #[cfg(test)]
        crate::userapp_lifecycle::common::transaction_fault_tests::check("admission_slots")?;
    }
    if let (Some(pg), UserAppAdmissionOutcome::Existing(operation)) = (pg, &result) {
        configuration::verify_captured_credentials(tx, operation, pg).await?;
    }
    if let Some(request_id) = &request.request_id {
        let operation = match &result {
            UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
        };
        if let Some(mapping) = mapping {
            if mapping.operation_id.as_deref() != Some(operation.operation_id.as_str()) {
                return Err(Error::VersionConflict);
            }
        } else {
            models::Request::create()
                .app_id(&operation.app_id)
                .request_id(request_id)
                .target_kind("control")
                .operation_id(Some(operation.operation_id.clone()))
                .lifecycle_id(Some(operation.lifecycle_id.clone()))
                .created_at_us(chrono::Utc::now().timestamp_micros())
                .exec(tx)
                .await
                .map_err(storage)?;
        }
    }

    #[cfg(test)]
    crate::userapp_lifecycle::common::transaction_fault_tests::check("admission_request")?;
    Ok(result)
}

//! Versioned private configuration. Callers execute these functions in the same
//! owned transaction as lifecycle admission, never across a runtime side effect.
use super::{codec, ops, repo};
use crate::{
    db::{models, schema::Backend},
    userapp_lifecycle::{domain, storage},
};
use shared_types::*;
use toasty::Executor;
use toasty_core::schema::db::Type;
type Error = UserAppStoreError;

fn invalid(message: &str) -> Error {
    Error::InvalidOperation(message.into())
}
fn scope_name(scope: UserAppOperationScope) -> Result<&'static str, Error> {
    if scope == UserAppOperationScope::Application {
        return Err(invalid("Runtime configuration requires dev or prod scope"));
    }
    Ok(codec::scope_name(scope))
}
async fn current_app(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    lifecycle_id: &str,
) -> Result<(), Error> {
    repo::claim_app(tx, backend, app_id).await?;
    let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    if app.lifecycle_id != lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    domain::validate_active(&app)
}
async fn head(
    tx: &mut dyn Executor,
    app_id: &str,
    lifecycle_id: &str,
    scope: UserAppOperationScope,
) -> Result<Option<models::RuntimeConfig>, Error> {
    models::RuntimeConfig::filter_by_app_id_and_lifecycle_id_and_scope(
        app_id,
        lifecycle_id,
        scope_name(scope)?,
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)
}
fn status(row: &models::RuntimeConfig) -> Result<RuntimeConfigurationStatus, Error> {
    if row.revision < 1
        || row.saved_version < 1
        || row.revision != row.saved_version
        || row
            .applied_version
            .is_some_and(|v| v < 1 || v > row.saved_version)
        || row
            .applying_version
            .is_some_and(|v| v < 1 || v > row.saved_version)
        || row.applying_version.is_some() != row.applying_operation_id.is_some()
    {
        return Err(invalid("Invalid persisted runtime configuration head"));
    }
    Ok(RuntimeConfigurationStatus {
        lifecycle_id: row.lifecycle_id.clone(),
        scope: codec::scope(&row.scope).map_err(storage)?,
        revision: row.revision,
        saved_version: row.saved_version,
        applied_version: row.applied_version,
        applying_version: row.applying_version,
        applying_operation_id: row.applying_operation_id.clone(),
        pending: row.applied_version != Some(row.saved_version),
    })
}
pub(super) async fn read_status(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    lifecycle_id: &str,
    scope: UserAppOperationScope,
) -> Result<Option<RuntimeConfigurationStatus>, Error> {
    current_app(tx, backend, app_id, lifecycle_id).await?;
    head(tx, app_id, lifecycle_id, scope)
        .await?
        .as_ref()
        .map(status)
        .transpose()
}
pub(super) async fn save(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    scope: UserAppOperationScope,
    request: &SaveRuntimeConfigurationRequest,
) -> Result<SavedRuntimeConfiguration, Error> {
    let scope = scope_name(scope)?;
    validate_identifier(&request.request_id, "request_id")
        .map_err(|_| invalid("Invalid configuration save request identity"))?;
    crate_validate_pg(&request.pg)?;
    if request.expected_revision < 0 {
        return Err(invalid("Configuration revision must not be negative"));
    }
    current_app(tx, backend, app_id, &request.lifecycle_id).await?;
    let scope_value = codec::scope(scope).map_err(storage)?;
    let previous = head(tx, app_id, &request.lifecycle_id, scope_value).await?;
    let fields = models::RuntimeConfigVersion::fields();
    let replay = models::RuntimeConfigVersion::filter(
        fields
            .app_id()
            .eq(app_id)
            .and(fields.lifecycle_id().eq(&request.lifecycle_id))
            .and(fields.scope().eq(scope))
            .and(fields.request_id().eq(&request.request_id)),
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)?;
    if let Some(replay) = replay {
        if replay.expected_revision != request.expected_revision
            || replay.pg_username != request.pg.username
            || replay.pg_password != request.pg.password
            || replay.payload_version != 1
        {
            return Err(invalid(
                "Configuration save identity was reused with different input",
            ));
        }
        let previous = previous
            .as_ref()
            .ok_or_else(|| invalid("Configuration replay has no current head"))?;
        return Ok(SavedRuntimeConfiguration {
            config_version: replay.version,
            status: status(previous)?,
        });
    }
    // Same root CAS as admission: a save must not turn an independent account
    // into a managed account while its explicit password write is in flight.
    let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    let active = repo::active(tx, &app).await?;
    if let Some(operation) = active.slot(scope_value)
        && matches!(
            operation.command,
            Some(UserAppControlCommand::ResetDatabasePassword { .. })
        )
    {
        return Err(Error::OperationInProgress(operation.blocker()));
    }
    let revision = previous
        .as_ref()
        .map(status)
        .transpose()?
        .map_or(0, |s| s.revision);
    if revision != request.expected_revision {
        return Err(Error::VersionConflict);
    }
    let version = revision
        .checked_add(1)
        .ok_or_else(|| invalid("Configuration version exhausted"))?;
    let now = chrono::Utc::now().timestamp_micros();
    models::RuntimeConfigVersion::create()
        .app_id(app_id)
        .lifecycle_id(&request.lifecycle_id)
        .scope(scope)
        .version(version)
        .request_id(&request.request_id)
        .expected_revision(revision)
        .payload_version(1)
        .pg_username(&request.pg.username)
        .pg_password(&request.pg.password)
        .created_at_us(now)
        .exec(tx)
        .await
        .map_err(storage)?;
    if previous.is_none() {
        models::RuntimeConfig::create()
            .app_id(app_id)
            .lifecycle_id(&request.lifecycle_id)
            .scope(scope)
            .revision(version)
            .saved_version(version)
            .applied_version(None::<i64>)
            .applying_version(None::<i64>)
            .applying_operation_id(None::<String>)
            .updated_at_us(now)
            .exec(tx)
            .await
            .map_err(storage)?;
    } else {
        let count = toasty::sql::statement(repo::sql(backend, "UPDATE userapp_runtime_configs SET revision=$1,saved_version=$2,updated_at_us=$3 WHERE app_id=$4 AND lifecycle_id=$5 AND scope=$6 AND revision=$7"))
            .bind(version).bind(version).bind(now).bind(app_id).bind(&request.lifecycle_id).bind(scope).bind(revision)
            .exec(tx).await.map_err(storage)?;
        if count != 1 {
            return Err(Error::VersionConflict);
        }
    }
    let row = head(tx, app_id, &request.lifecycle_id, scope_value)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(SavedRuntimeConfiguration {
        config_version: version,
        status: status(&row)?,
    })
}

/// Invoked in the admission transaction after its root CAS, before any slot or
/// operation is committed. Never holds a database transaction during remote SQL.
pub(super) async fn guard_database_admin(
    tx: &mut dyn Executor,
    operation: &UserAppOperationRecord,
) -> Result<(), Error> {
    if !matches!(
        operation.kind,
        UserAppOperationKind::ResetDevDatabasePassword
            | UserAppOperationKind::ResetProdDatabasePassword
    ) {
        return Ok(());
    }
    let Some(UserAppControlCommand::ResetDatabasePassword { username, .. }) = &operation.command
    else {
        return Err(invalid(
            "Database administration requires an explicit target account",
        ));
    };
    pg_utils::validate_pg_identifier(username).map_err(Error::InvalidOperation)?;
    let Some(row) = head(
        tx,
        &operation.app_id,
        &operation.lifecycle_id,
        operation.scope,
    )
    .await?
    else {
        return Ok(());
    };
    status(&row)?;
    // Saved, applied and uncertain-in-flight versions can reference distinct
    // accounts. Checking only the newest save would expose the running account.
    for version in [
        Some(row.saved_version),
        row.applied_version,
        row.applying_version,
    ]
    .into_iter()
    .flatten()
    {
        let configuration =
            models::RuntimeConfigVersion::filter_by_app_id_and_lifecycle_id_and_scope_and_version(
                &operation.app_id,
                &operation.lifecycle_id,
                &row.scope,
                version,
            )
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
            .ok_or_else(|| invalid("Managed configuration version is missing"))?;
        if configuration.payload_version != 1 {
            return Err(invalid("Unknown runtime configuration payload version"));
        }
        if configuration.pg_username == *username {
            return Err(invalid(
                "Managed runtime accounts must use the runtime configuration API",
            ));
        }
    }
    Ok(())
}
fn crate_validate_pg(pg: &StartPgCredential) -> Result<(), Error> {
    pg_utils::validate_pg_identifier(&pg.username).map_err(Error::InvalidOperation)?;
    if pg.password.is_empty() || pg.password.contains('\0') {
        return Err(invalid(
            "PG password must be nonempty and contain no NUL bytes",
        ));
    }
    Ok(())
}

/// Runs only for newly accepted operations. Replays never recapture even when
/// the original operation had no configured credentials (absence is immutable).
async fn verify_version_credentials(
    tx: &mut dyn Executor,
    op: &UserAppOperationRecord,
    version: i64,
    pg: &StartPgCredential,
) -> Result<(), Error> {
    let row =
        models::RuntimeConfigVersion::filter_by_app_id_and_lifecycle_id_and_scope_and_version(
            &op.app_id,
            &op.lifecycle_id,
            scope_name(op.scope)?,
            version,
        )
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| invalid("Deployment configuration version is missing"))?;
    if row.payload_version != 1 || row.pg_username != pg.username || row.pg_password != pg.password
    {
        return Err(invalid(
            "Explicit deployment credentials differ from the saved configuration",
        ));
    }
    Ok(())
}

pub(super) async fn verify_captured_credentials(
    tx: &mut dyn Executor,
    op: &UserAppOperationRecord,
    pg: &StartPgCredential,
) -> Result<(), Error> {
    let row = receipt(tx, op)
        .await?
        .ok_or_else(|| invalid("Replayed deployment has no captured credentials"))?;
    verify_version_credentials(tx, op, row.config_version, pg).await
}

/// Called after admission validation, before capture, in the same root-CAS tx.
pub(super) async fn seed_deployment_credentials(
    tx: &mut dyn Executor,
    backend: Backend,
    op: &UserAppOperationRecord,
    pg: &StartPgCredential,
) -> Result<(), Error> {
    crate_validate_pg(pg)?;
    if let Some(existing) = head(tx, &op.app_id, &op.lifecycle_id, op.scope).await? {
        let summary = status(&existing)?;
        return verify_version_credentials(tx, op, summary.saved_version, pg).await;
    }
    use sha2::{Digest as _, Sha256};
    let suffix: String =
        Sha256::digest(format!("deployment-configuration:{}", op.operation_id).as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
    save(
        tx,
        backend,
        &op.app_id,
        op.scope,
        &SaveRuntimeConfigurationRequest {
            lifecycle_id: op.lifecycle_id.clone(),
            request_id: suffix,
            expected_revision: 0,
            pg: pg.clone(),
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn capture(
    tx: &mut dyn Executor,
    op: &UserAppOperationRecord,
) -> Result<(), Error> {
    use UserAppOperationKind as Kind;
    if !matches!(
        op.kind,
        Kind::EnsureBuilder
            | Kind::RestartBuilder
            | Kind::Create
            | Kind::StartDeployment
            | Kind::RestartDeployment
            | Kind::Start
            | Kind::Restart
            | Kind::HotDeploy
    ) {
        return Ok(());
    }
    let Some(row) = head(tx, &op.app_id, &op.lifecycle_id, op.scope).await? else {
        return Ok(());
    };
    let summary = status(&row)?;
    if summary.applying_operation_id.is_some() {
        // A cleared lifecycle slot is not proof that credential writes finished.
        return Err(invalid("Runtime credential application requires recovery"));
    }
    if op.kind == Kind::HotDeploy && summary.pending {
        return Err(invalid(
            "Runtime credential changes require a cold deployment",
        ));
    }
    let applied_only = matches!(
        op.command,
        Some(UserAppControlCommand::Start { traffic: true })
    ) || op.kind == Kind::EnsureBuilder
        || op.kind == Kind::HotDeploy;
    let version = if applied_only {
        row.applied_version
    } else {
        Some(row.saved_version)
    };
    let Some(version) = version else {
        return Ok(());
    };
    let now = chrono::Utc::now().timestamp_micros();
    models::OperationConfig::create()
        .operation_id(&op.operation_id)
        .app_id(&op.app_id)
        .lifecycle_id(&op.lifecycle_id)
        .scope(codec::scope_name(op.scope))
        .config_version(version)
        .physical_uid(None::<String>)
        .deployment_generation(None::<String>)
        .credential_state("captured")
        .business_state("not_started")
        .created_at_us(now)
        .updated_at_us(now)
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(())
}
async fn owned(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
) -> Result<UserAppOperationRecord, Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let (_, op) = ops::current(
        tx,
        backend,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    if !ops::owns(context, &op) || op.state != UserAppOperationState::Running {
        return Err(Error::VersionConflict);
    }
    Ok(op)
}
async fn receipt(
    tx: &mut dyn Executor,
    op: &UserAppOperationRecord,
) -> Result<Option<models::OperationConfig>, Error> {
    let row = models::OperationConfig::filter_by_operation_id(&op.operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
    if let Some(row) = &row
        && (row.app_id != op.app_id
            || row.lifecycle_id != op.lifecycle_id
            || row.scope != codec::scope_name(op.scope)
            || row.config_version < 1
            || row.physical_uid.is_some() != row.deployment_generation.is_some())
    {
        return Err(invalid("Runtime configuration receipt identity mismatch"));
    }
    Ok(row)
}
fn credentials(value: &str) -> Result<CredentialApplicationState, Error> {
    use CredentialApplicationState::*;
    match value {
        "captured" => Ok(Captured),
        "applying" => Ok(Applying),
        "applied" => Ok(Applied),
        "failed" => Ok(Failed),
        "unknown" => Ok(Unknown),
        _ => Err(invalid("Unknown persisted credential state")),
    }
}
fn credential_name(value: CredentialApplicationState) -> &'static str {
    use CredentialApplicationState::*;
    match value {
        Captured => "captured",
        Applying => "applying",
        Applied => "applied",
        Failed => "failed",
        Unknown => "unknown",
    }
}
fn business(value: &str) -> Result<BusinessStartupState, Error> {
    use BusinessStartupState::*;
    match value {
        "not_started" => Ok(NotStarted),
        "starting" => Ok(Starting),
        "ready" => Ok(Ready),
        "failed" => Ok(Failed),
        "unknown" => Ok(Unknown),
        _ => Err(invalid("Unknown persisted business state")),
    }
}
fn business_name(value: BusinessStartupState) -> &'static str {
    use BusinessStartupState::*;
    match value {
        NotStarted => "not_started",
        Starting => "starting",
        Ready => "ready",
        Failed => "failed",
        Unknown => "unknown",
    }
}
pub(super) async fn read(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
) -> Result<Option<RuntimeConfigurationCapture>, Error> {
    let op = owned(tx, backend, context).await?;
    let Some(row) = receipt(tx, &op).await? else {
        return Ok(None);
    };
    let version =
        models::RuntimeConfigVersion::filter_by_app_id_and_lifecycle_id_and_scope_and_version(
            &row.app_id,
            &row.lifecycle_id,
            &row.scope,
            row.config_version,
        )
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| invalid("Captured configuration version is missing"))?;
    if version.payload_version != 1 {
        return Err(invalid("Unknown runtime configuration payload version"));
    }
    let pg = StartPgCredential {
        username: version.pg_username,
        password: version.pg_password,
    };
    crate_validate_pg(&pg)?;
    let target = row.physical_uid.zip(row.deployment_generation).map(
        |(physical_uid, deployment_generation)| RuntimeConfigurationTarget {
            physical_uid,
            deployment_generation,
        },
    );
    Ok(Some(RuntimeConfigurationCapture {
        operation_id: row.operation_id,
        lifecycle_id: row.lifecycle_id,
        scope: op.scope,
        config_version: row.config_version,
        pg,
        target,
        credentials: credentials(&row.credential_state)?,
        business: business(&row.business_state)?,
    }))
}
pub(super) async fn bind(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
    version: i64,
    target: &RuntimeConfigurationTarget,
) -> Result<(), Error> {
    if target.physical_uid.is_empty() || target.deployment_generation.is_empty() {
        return Err(invalid("Runtime configuration target identity is empty"));
    }
    let op = owned(tx, backend, context).await?;
    let row = receipt(tx, &op).await?.ok_or(Error::NotFound)?;
    if row.config_version != version {
        return Err(Error::VersionConflict);
    }
    if let Some(uid) = row.physical_uid {
        return if uid == target.physical_uid
            && row.deployment_generation.as_deref() == Some(&target.deployment_generation)
        {
            Ok(())
        } else {
            Err(Error::VersionConflict)
        };
    }
    if row.credential_state != "captured" {
        return Err(invalid("Unbound configuration is not captured"));
    }
    let count = toasty::sql::statement(repo::sql(backend, "UPDATE userapp_operation_configs SET physical_uid=$1,deployment_generation=$2,updated_at_us=$3 WHERE operation_id=$4 AND physical_uid IS NULL"))
        .bind(&target.physical_uid).bind(&target.deployment_generation).bind(chrono::Utc::now().timestamp_micros()).bind(&op.operation_id)
        .exec(tx).await.map_err(storage)?;
    if count != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
fn validate_transition(
    old: CredentialApplicationState,
    new: CredentialApplicationState,
    before: BusinessStartupState,
    after: BusinessStartupState,
) -> Result<(), Error> {
    use BusinessStartupState as B;
    use CredentialApplicationState as C;
    let valid = old == new
        || matches!(
            (old, new),
            (C::Captured, C::Applying | C::Failed)
                // Applying is durable intent before command dispatch. A typed
                // NotAttempted result (validation/read-only preflight failure)
                // can confirm Failed; uncertain writes must instead use Unknown.
                | (C::Applying, C::Applied | C::Failed | C::Unknown)
                | (C::Unknown, C::Applied)
        );
    let valid_business = before == after
        || matches!(
            (before, after),
            (B::NotStarted, B::Starting | B::Failed)
                | (B::Starting, B::Ready | B::Failed | B::Unknown)
                | (B::Unknown, B::Ready | B::Failed)
        );
    if !valid
        || !valid_business
        || (new != C::Applied && !matches!(after, B::NotStarted | B::Failed))
    {
        return Err(invalid("Invalid runtime configuration result transition"));
    }
    Ok(())
}
pub(super) async fn record(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
    version: i64,
    target: &RuntimeConfigurationTarget,
    credential: CredentialApplicationState,
    startup: BusinessStartupState,
) -> Result<(), Error> {
    let op = owned(tx, backend, context).await?;
    let row = receipt(tx, &op).await?.ok_or(Error::NotFound)?;
    if row.config_version != version
        || row.physical_uid.as_deref() != Some(&target.physical_uid)
        || row.deployment_generation.as_deref() != Some(&target.deployment_generation)
    {
        return Err(Error::VersionConflict);
    }
    validate_transition(
        credentials(&row.credential_state)?,
        credential,
        business(&row.business_state)?,
        startup,
    )?;
    let head = head(tx, &op.app_id, &op.lifecycle_id, op.scope)
        .await?
        .ok_or(Error::NotFound)?;
    status(&head)?;
    if head
        .applying_operation_id
        .as_deref()
        .is_some_and(|id| id != op.operation_id)
    {
        return Err(Error::VersionConflict);
    }
    let now = chrono::Utc::now().timestamp_micros();
    let count = toasty::sql::statement(repo::sql(backend, "UPDATE userapp_operation_configs SET credential_state=$1,business_state=$2,updated_at_us=$3 WHERE operation_id=$4 AND credential_state=$5 AND business_state=$6"))
        .bind(credential_name(credential)).bind(business_name(startup)).bind(now).bind(&op.operation_id)
        .bind(&row.credential_state).bind(&row.business_state).exec(tx).await.map_err(storage)?;
    if count != 1 {
        return Err(Error::VersionConflict);
    }
    let applying = matches!(
        credential,
        CredentialApplicationState::Applying | CredentialApplicationState::Unknown
    );
    let applied = if credential == CredentialApplicationState::Applied {
        Some(version)
    } else {
        head.applied_version
    };
    let count = toasty::sql::statement(repo::sql(backend, "UPDATE userapp_runtime_configs SET applied_version=$1,applying_version=$2,applying_operation_id=$3,updated_at_us=$4 WHERE app_id=$5 AND lifecycle_id=$6 AND scope=$7 AND revision=$8"))
        .bind_typed(applied, Type::Integer(8)).bind_typed(applying.then_some(version), Type::Integer(8))
        .bind_typed(applying.then_some(op.operation_id.as_str()), Type::Text).bind(now).bind(&op.app_id)
        .bind(&op.lifecycle_id).bind(codec::scope_name(op.scope)).bind(head.revision).exec(tx).await.map_err(storage)?;
    if count != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}

/// Storage-side backstop: no caller can finalize an unresolved credential write,
/// or report startup success before both durable observations have succeeded.
pub(super) async fn validate_terminal(
    tx: &mut dyn Executor,
    op: &UserAppOperationRecord,
    state: UserAppOperationState,
) -> Result<(), Error> {
    if !state.is_terminal() {
        return Ok(());
    }
    let Some(row) = receipt(tx, op).await? else {
        return Ok(());
    };
    let credentials = credentials(&row.credential_state)?;
    let business = business(&row.business_state)?;
    if matches!(
        credentials,
        CredentialApplicationState::Applying | CredentialApplicationState::Unknown
    ) || matches!(
        business,
        BusinessStartupState::Starting | BusinessStartupState::Unknown
    ) {
        return Err(invalid(
            "Uncertain runtime configuration outcome requires recovery",
        ));
    }
    if state == UserAppOperationState::Succeeded
        && (credentials != CredentialApplicationState::Applied
            || business != BusinessStartupState::Ready)
    {
        return Err(invalid(
            "Runtime credentials and business readiness are not confirmed",
        ));
    }
    Ok(())
}

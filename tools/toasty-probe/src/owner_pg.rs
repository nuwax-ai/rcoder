//! Real PostgreSQL checks for the same owner source used by production storage.
use super::{
    one_bool,
    owner::DatabaseOwner,
    policy_driver::{ConnectionPolicy, PolicyDriver},
};
#[cfg(feature = "common-contract")]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::time::Duration;
use tokio::sync::oneshot;

pub async fn run(url: String) -> Result<()> {
    let connection_url = url.clone();
    let owner = DatabaseOwner::open(16, 4, (), move || async move {
        let driver = PolicyDriver::new(
            toasty::db::Connect::new(&connection_url).await?,
            ConnectionPolicy::Postgres {
                statement_timeout_ms: 5000,
            },
        );
        Ok(toasty::Db::builder().max_pool_size(4).build(driver).await?)
    })
    .await?;
    owner
        .execute(|mut db| async move {
            toasty::sql::statement(
                "CREATE TABLE t0_owner_cas (id BIGINT PRIMARY KEY, revision BIGINT NOT NULL)",
            )
            .exec(&mut db)
            .await?;
            toasty::sql::statement("INSERT INTO t0_owner_cas VALUES (1,0)")
                .exec(&mut db)
                .await?;
            Ok(())
        })
        .await?;
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let owner = owner.clone();
        jobs.spawn(async move {
            owner
                .execute(|mut db| async move {
                    let mut tx = db.transaction().await?;
                    let changed = toasty::sql::statement(
                        "UPDATE t0_owner_cas SET revision=1 WHERE id=1 AND revision=0",
                    )
                    .exec(&mut tx)
                    .await?;
                    tx.commit().await?;
                    Ok(changed)
                })
                .await
        });
    }
    let mut winners = 0;
    while let Some(result) = jobs.join_next().await {
        winners += result??;
    }
    ensure!(winners == 1, "concurrent CAS must have exactly one winner");

    let (acquired, ready) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let leader = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .execute(move |db| async move {
                    let mut connection = db.connection().await?;
                    toasty::sql::statement("SELECT pg_advisory_lock(123654780)")
                        .exec(&mut connection)
                        .await?;
                    acquired
                        .send(())
                        .map_err(|_| anyhow::anyhow!("probe observer disappeared"))?;
                    released.await?;
                    // Deliberately omit unlock: only destruction of the official driver's
                    // transport on the owner runtime can release this session lock.
                    Ok(())
                })
                .await
        }
    });
    ready.await?;
    let mut rival = toasty::Db::builder().max_pool_size(1).connect(&url).await?;
    ensure!(!one_bool(
        &toasty::sql::query("SELECT pg_try_advisory_lock(123654780)")
            .exec(&mut rival)
            .await?
    )?);
    let mut shutdown = Box::pin(owner.shutdown());
    ensure!(futures::poll!(&mut shutdown).is_pending());
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("leader exited prematurely"))?;
    leader.await??;
    shutdown.await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if one_bool(
                &toasty::sql::query("SELECT pg_try_advisory_lock(123654780)")
                    .exec(&mut rival)
                    .await?,
            )? {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    toasty::sql::query("SELECT pg_advisory_unlock(123654780)")
        .exec(&mut rival)
        .await?;
    println!(
        "PASS official PG driver, concurrent transaction CAS, drained shutdown, physical advisory-lock release"
    );
    Ok(())
}

pub async fn schema(url: String) -> Result<()> {
    use super::{
        storage_models,
        storage_schema::{self, Backend, Component},
    };
    let mut admin = toasty::Db::builder().connect(&url).await?;
    toasty::sql::statement("CREATE SCHEMA t0_baseline")
        .exec(&mut admin)
        .await?;
    let url = format!("{url}&options=-csearch_path%3Dt0_baseline");
    let mut owners = Vec::new();
    for _ in 0..2 {
        let url = url.clone();
        owners.push(
            DatabaseOwner::open(8, 4, (), move || async move {
                let driver = PolicyDriver::new(
                    toasty::db::Connect::new(&url).await?,
                    ConnectionPolicy::Postgres {
                        statement_timeout_ms: 10000,
                    },
                );
                Ok(toasty::Db::builder()
                    .models(storage_models::storage_models())
                    .max_pool_size(4)
                    .build(driver)
                    .await?)
            })
            .await?,
        );
    }
    let mut jobs = tokio::task::JoinSet::new();
    for owner in &owners {
        let owner = owner.clone();
        jobs.spawn(async move {
            owner
                .execute(|mut db| async move {
                    storage_schema::initialize(
                        &mut db,
                        Backend::Postgres,
                        &[Component::Userapp, Component::Project, Component::Preview],
                    )
                    .await
                })
                .await
        });
    }
    while let Some(result) = jobs.join_next().await {
        result??;
    }
    owners[0]
        .execute(|mut db| async move {
            storage_schema::initialize(
                &mut db,
                Backend::Postgres,
                &[Component::Userapp, Component::Project, Component::Preview],
            )
            .await?;
            let ledger = toasty::sql::query("SELECT component FROM rcoder_schema_migrations")
                .exec(&mut db)
                .await?;
            ensure!(ledger.len() == 3, "each component must be installed once");
            storage_models::Application::create()
                .app_id("app")
                .lifecycle_id("life")
                .lifecycle_epoch(1)
                .lifecycle_state("active")
                .metadata_revision(1)
                .created_at_us(1)
                .updated_at_us(1)
                .exec(&mut db)
                .await?;
            ensure!(
                storage_models::Application::get_by_app_id(&mut db, "app")
                    .await?
                    .lifecycle_id
                    == "life"
            );
            toasty::sql::statement("DROP INDEX preview_active_port_unique")
                .exec(&mut db)
                .await?;
            ensure!(
                storage_schema::initialize(&mut db, Backend::Postgres, &[Component::Preview])
                    .await
                    .is_err(),
                "missing unique port constraint was not rejected"
            );
            Ok(())
        })
        .await?;
    for owner in owners {
        owner.shutdown().await?;
    }
    println!(
        "PASS two PostgreSQL owners initialize all three components concurrently; model roundtrip; catalog drift rejects"
    );
    Ok(())
}

#[cfg(feature = "common-contract")]
pub async fn common_contract(url: String) -> Result<()> {
    use shared_types::{
        UserAppAdmission, UserAppAdmissionOutcome as Outcome, UserAppLifecycleStore as _,
        UserAppOperationKind as Kind, UserAppOperationProgress, UserAppOperationState as State,
    };
    use std::sync::Arc;
    let mut admin = toasty::Db::builder().connect(&url).await?;
    let name = format!("t0_contract_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {name}"))
        .exec(&mut admin)
        .await?;
    let config = crate::config::PostgresConfig {
        url: Some(format!("{url}&options=-csearch_path%3D{name}")),
        ..Default::default()
    };
    let mut stores = Vec::new();
    for _ in 0..2 {
        let owner =
            crate::db::postgres::open(&config, vec![crate::storage_schema::Component::Userapp])
                .await?;
        stores.push(Arc::new(
            crate::userapp_lifecycle::TursoUserAppStore::from_owner(
                owner,
                crate::storage_schema::Backend::Postgres,
            ),
        ));
    }
    let make_request = |id: &str, kind| UserAppAdmission {
        app_id: "pgapp".into(),
        lifecycle_id: None,
        operation_id: id.into(),
        request_id: Some(id.into()),
        request_fingerprint: "a".repeat(64),
        kind,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    };
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..8 {
        let store = stores[index % 2].clone();
        let request = make_request(&format!("operation{index}"), Kind::EnsureBuilder);
        tasks.spawn(async move { store.admit(&request).await });
    }
    let mut accepted = 0;
    let mut operation = None;
    while let Some(result) = tasks.join_next().await {
        let outcome = result??;
        let op = match outcome {
            Outcome::Accepted(op) => {
                accepted += 1;
                op
            }
            Outcome::Existing(op) => op,
        };
        if let Some(previous) = &operation {
            ensure!(previous == &op.operation_id);
        } else {
            operation = Some(op.operation_id);
        }
    }
    ensure!(
        accepted == 1,
        "two owners must admit exactly one builder execution"
    );
    let prod = match stores[1].admit(&make_request("prod", Kind::Create)).await? {
        Outcome::Accepted(op) => op,
        _ => anyhow::bail!("prod admission unexpectedly joined dev"),
    };
    let progress = UserAppOperationProgress {
        app_id: prod.app_id.clone(),
        operation_id: prod.operation_id.clone(),
        lifecycle_id: prod.lifecycle_id.clone(),
        expected_revision: prod.revision,
        executor_id: "executor".into(),
        state: State::Running,
        step: "claimed".into(),
        checkpoint: serde_json::Value::Null,
        error_code: None,
        error_message: None,
    };
    let (a, b) = tokio::join!(stores[0].advance(&progress), stores[1].advance(&progress));
    ensure!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()) == 1,
        "revision CAS must reject a competing executor claim"
    );
    let op = stores[0]
        .get_operation("pgapp", "prod")
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing prod operation"))?;
    let mut wrong = progress.clone();
    wrong.expected_revision = op.revision;
    wrong.executor_id = "other".into();
    ensure!(
        stores[1].advance(&wrong).await.is_err(),
        "another executor advanced an owned operation"
    );
    let snapshots = stores[1].list_control_snapshots(None, 10).await?;
    ensure!(
        snapshots.len() == 1
            && snapshots[0].application.active_operations.dev.is_some()
            && snapshots[0].application.active_operations.prod.is_some()
    );
    // Exercise the same version-capture and CAS algorithm across independent
    // connections/owners, not only within the single-worker Turso backend.
    use shared_types::{
        BusinessStartupState as Business, CredentialApplicationState as Credential,
        RuntimeConfigurationTarget, SaveRuntimeConfigurationRequest, StartPgCredential,
        UserAppControlCommand, UserAppExecutionContext, UserAppOperationScope::Prod,
        UserAppRuntimeConfigurationStore as _,
    };
    let config_app = stores[0].ensure_identity("pgconfigapp").await?;
    let save = |id: &str, password: &str, revision| SaveRuntimeConfigurationRequest {
        lifecycle_id: config_app.lifecycle_id.clone(),
        request_id: id.into(),
        expected_revision: revision,
        pg: StartPgCredential {
            username: "business".into(),
            password: password.into(),
        },
    };
    let left = save("firstleft", "left", 0);
    let right = save("firstright", "right", 0);
    let (a, b) = tokio::join!(
        stores[0].save_runtime_configuration("pgconfigapp", Prod, &left),
        stores[1].save_runtime_configuration("pgconfigapp", Prod, &right)
    );
    ensure!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()) == 1,
        "PG configuration CAS admitted two winners"
    );
    let request = UserAppAdmission {
        app_id: "pgconfigapp".into(),
        lifecycle_id: Some(config_app.lifecycle_id.clone()),
        command: Some(UserAppControlCommand::Start { traffic: false }),
        ..make_request("configstart", Kind::Start)
    };
    let op = match stores[0].admit(&request).await? {
        Outcome::Accepted(op) => op,
        _ => anyhow::bail!("unexpected configuration replay"),
    };
    let mut config_progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        ..progress.clone()
    };
    let running = stores[1].advance(&config_progress).await?;
    let context = UserAppExecutionContext {
        app_id: running.app_id.clone(),
        lifecycle_id: running.lifecycle_id.clone(),
        operation_id: running.operation_id.clone(),
        executor_id: "executor".into(),
        request_fingerprint: running.request_fingerprint.clone(),
    };
    stores[1]
        .save_runtime_configuration("pgconfigapp", Prod, &save("second", "next", 1))
        .await?;
    let capture = stores[0]
        .operation_runtime_configuration(&context)
        .await?
        .context("missing capture")?;
    ensure!(
        capture.config_version == 1 && capture.pg.password != "next",
        "save overwrote an executing capture"
    );
    let target = RuntimeConfigurationTarget {
        physical_uid: "uidone".into(),
        deployment_generation: "generationone".into(),
    };
    stores[0]
        .bind_runtime_configuration_target(&context, 1, &target)
        .await?;
    stores[1]
        .record_runtime_configuration_result(
            &context,
            1,
            &target,
            Credential::Applying,
            Business::NotStarted,
        )
        .await?;
    stores[1]
        .record_runtime_configuration_result(
            &context,
            1,
            &target,
            Credential::Unknown,
            Business::NotStarted,
        )
        .await?;
    config_progress.expected_revision = running.revision;
    config_progress.state = State::Failed;
    ensure!(
        stores[0].advance(&config_progress).await.is_err(),
        "unknown credential write was finalized"
    );
    stores[1]
        .record_runtime_configuration_result(
            &context,
            1,
            &target,
            Credential::Applied,
            Business::Starting,
        )
        .await?;
    stores[1]
        .record_runtime_configuration_result(
            &context,
            1,
            &target,
            Credential::Applied,
            Business::Failed,
        )
        .await?;
    stores[0].advance(&config_progress).await?;
    let status = stores[0]
        .runtime_configuration_status("pgconfigapp", &config_app.lifecycle_id, Prod)
        .await?
        .context("missing status")?;
    ensure!(status.applied_version == Some(1) && status.saved_version == 2 && status.pending);
    ensure!(status.applying_operation_id.is_none());
    println!(
        "PASS PostgreSQL runtime configuration: cross-owner save CAS, immutable admission capture, uncertain-write fence, applied credentials survive business failure"
    );
    stores[0].shutdown().await?;
    ensure!(
        stores[1].get_operation("pgapp", "prod").await?.is_some(),
        "closing one store affected its independent peer"
    );
    stores[1].shutdown().await?;
    println!(
        "PASS shared PostgreSQL lifecycle algorithm: two owners, coalesced admission, dev/prod isolation, revision/executor fencing, consistent snapshot, independent shutdown"
    );
    Ok(())
}

#[cfg(feature = "common-contract")]
pub async fn preview_contract(url: String) -> Result<()> {
    use shared_types::{
        AcceptStartInput, AcceptStartOutcome, PreviewHostIdentity, PreviewLifecycleStore as _,
    };
    use std::sync::Arc;
    let mut admin = toasty::Db::builder().connect(&url).await?;
    let name = format!("t0_preview_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {name}"))
        .exec(&mut admin)
        .await?;
    let config = crate::config::PostgresConfig {
        url: Some(format!("{url}&options=-csearch_path%3D{name}")),
        ..Default::default()
    };
    let a = Arc::new(crate::preview_lifecycle::PgPreviewStore::connect(&config).await?);
    let b = Arc::new(crate::preview_lifecycle::PgPreviewStore::connect(&config).await?);
    crate::preview_lifecycle::contract_suite::run(a.as_ref()).await;
    let make = |key: &str, id: &str, requested_port| AcceptStartInput {
        preview_key: key.into(),
        project_id: "previewproject".into(),
        project_path: "/workspace/previewproject".into(),
        host: PreviewHostIdentity {
            host_id: "pod:boot".into(),
            pod_name: None,
            pod_ip: None,
        },
        operation_id: format!("operation{id}"),
        instance_id: format!("instance{id}"),
        requested_port,
        recover_unknown_evidence: None,
    };
    let (left, right) = tokio::join!(
        a.accept_start(make("samekey", "one", None)),
        b.accept_start(make("samekey", "two", None))
    );
    ensure!(
        usize::from(matches!(left, Ok(AcceptStartOutcome::Admitted(_))))
            + usize::from(matches!(right, Ok(AcceptStartOutcome::Admitted(_))))
            == 1,
        "same-key initial start overwrote a concurrent winner"
    );
    let occupied = a.active_ports().await?;
    let port = (shared_types::PREVIEW_PORT_MIN..=shared_types::PREVIEW_PORT_MAX)
        .find(|p| shared_types::is_preview_port(*p) && !occupied.contains(p))
        .context("no free test port")?;
    let (left, right) = tokio::join!(
        a.accept_start(make("portkeyone", "three", Some(port))),
        b.accept_start(make("portkeytwo", "four", Some(port)))
    );
    ensure!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()) == 1,
        "same port allocated twice"
    );
    let active = a.active_ports().await?;
    ensure!(active.iter().filter(|p| **p == port).count() == 1);
    a.close().await?;
    b.close().await?;
    println!(
        "PASS Toasty PostgreSQL preview: complete shared contract, concurrent same-key creation, different-key port race"
    );
    Ok(())
}

//! Verified replacement and explicit adoption rebind the builder registry and
//! lifecycle outcome in one short transaction, before publishing the memory view.
use super::{
    load,
    persist_ops::{ContainerSnapshot, ProjectSnapshot},
    repo,
};
use crate::{db::models, pg::PgStore};
use anyhow::{Context, Result, ensure};
use shared_types::{
    AppResourceIdentity, BuilderCreationEvidence, ProjectAndContainerInfo, ServiceType,
    UserAppOperationRecord,
    persistence::{ContainerPersistenceIdentity, PersistenceOperationOutcome},
};
use std::sync::{Arc, atomic::Ordering};
use toasty::Executor;

struct InFlight<'a>(&'a PgStore);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.active_writes.fetch_sub(1, Ordering::AcqRel);
        self.0.write_finished.notify_waiters();
    }
}

impl PgStore {
    pub async fn completed_builder_registration_candidates(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        pod_uid: &str,
        workload_uid: &str,
    ) -> Result<Vec<UserAppOperationRecord>> {
        let app_id = app_id.to_owned();
        let lifecycle_id = lifecycle_id.to_owned();
        let pod_uid = pod_uid.to_owned();
        let workload_uid = workload_uid.to_owned();
        super::database::read(&self.database, move |tx| {
            Box::pin(async move {
                crate::userapp_lifecycle::completed_builder_registration_candidates(
                    tx,
                    &app_id,
                    &lifecycle_id,
                    &pod_uid,
                    &workload_uid,
                )
                .await
            })
        })
        .await
    }

    pub async fn register_completed_builder_replacement(
        &self,
        operation: &UserAppOperationRecord,
        evidence: &BuilderCreationEvidence,
        volumes: &[AppResourceIdentity],
    ) -> Result<()> {
        let baseline = {
            let _registration = self
                .registration
                .lock()
                .map_err(|_| anyhow::anyhow!("Project registration lock poisoned"))?;
            ensure!(
                !self.closing.load(Ordering::Acquire),
                "Persistence is shutting down"
            );
            let baseline = self.inner.get(&operation.app_id);
            self.active_writes.fetch_add(1, Ordering::AcqRel);
            baseline
        };
        let _in_flight = InFlight(self);
        let operation = operation.clone();
        let evidence = evidence.clone();
        let volumes = volumes.to_vec();
        let committed = self.database.execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            let result = rebind(&mut tx, &operation, &evidence, &volumes).await;
            match result {
                Ok(info) => { tx.commit().await?; Ok(info) }
                Err(error) => {
                    tx.rollback().await.map_err(|rollback| anyhow::anyhow!(
                        "Builder registration rollback failed: {rollback}; original error: {error}"))?;
                    Err(error)
                }
            }
        }).await.context("Commit verified builder registry replacement")?;
        self.publish_rebound_builder(baseline, committed)
    }

    /// Registration-only repair for an explicitly adopted legacy workload.
    /// The original creation history stays untouched; registry and this new
    /// adoption outcome commit together under the application's root token.
    pub async fn complete_builder_registration_adoption(
        &self,
        expected: &ProjectAndContainerInfo,
        target: &shared_types::BuilderControlTarget,
        container: &shared_types::ContainerBasicInfo,
        volumes: &[AppResourceIdentity],
        progress: &shared_types::UserAppOperationProgress,
    ) -> Result<()> {
        target.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            progress.checkpoint.get("registration_target") == Some(&serde_json::to_value(target)?)
                && progress.checkpoint.get("registration_volumes")
                    == Some(&serde_json::to_value(volumes)?)
                && progress.checkpoint.get("container") == Some(&serde_json::to_value(container)?),
            "Builder adoption checkpoint differs from the inspected registry evidence"
        );
        ensure!(
            !volumes.is_empty()
                && volumes.iter().all(|volume| volume.kind
                    == shared_types::AppResourceKind::PersistentVolumeClaim
                    && !volume.name.is_empty()
                    && !volume.uid.is_empty())
                && volumes
                    .iter()
                    .map(|volume| &volume.name)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == volumes.len(),
            "Explicit builder registration adoption requires current PVC identities"
        );
        ensure!(
            expected.project_id() == target.context.app_id,
            "Old builder registry belongs to another application"
        );
        let expected_source_uid = expected
            .container_info()
            .and_then(|basic| basic.workload_uid)
            .context("Old builder registration has no workload UID")?;
        let baseline = {
            let _registration = self
                .registration
                .lock()
                .map_err(|_| anyhow::anyhow!("Project registration lock poisoned"))?;
            ensure!(
                !self.closing.load(Ordering::Acquire),
                "Persistence is shutting down"
            );
            let baseline = self.inner.get(expected.project_id());
            self.active_writes.fetch_add(1, Ordering::AcqRel);
            baseline
        };
        let _in_flight = InFlight(self);
        let expected = expected.clone();
        let target = target.clone();
        let container = container.clone();
        let progress = progress.clone();
        let committed = self.database.execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            let result = async {
                crate::userapp_lifecycle::commit_builder_registration_adoption(&mut tx, &target, &expected_source_uid, &progress).await?;
                rebind_rows(&mut tx, &target.context, &target, &container, None, Some(&expected)).await
            }.await;
            match result {
                Ok(info) => { tx.commit().await?; Ok(info) }
                Err(error) => {
                    tx.rollback().await.map_err(|rollback| anyhow::anyhow!("Builder adoption rollback failed: {rollback}; original error: {error}"))?;
                    Err(error)
                }
            }
        }).await.context("Commit explicit builder registration adoption")?;
        self.publish_rebound_builder(baseline, committed)
    }

    fn publish_rebound_builder(
        &self,
        baseline: Option<Arc<ProjectAndContainerInfo>>,
        committed: ProjectAndContainerInfo,
    ) -> Result<()> {
        // Publish only after the database transaction commits. A concurrent
        // local project replacement must never be overwritten by its response.
        let registration = self
            .registration
            .lock()
            .map_err(|_| anyhow::anyhow!("Project registration lock poisoned"))?;
        let current = self.inner.get(committed.project_id());
        if current.as_ref().is_some_and(|current| {
            current.persistence_identity().generation == committed.persistence_identity().generation
                && current
                    .persistence_identity()
                    .container
                    .as_ref()
                    .map(|identity| &identity.generation)
                    == committed
                        .persistence_identity()
                        .container
                        .as_ref()
                        .map(|identity| &identity.generation)
                && current
                    .container_info()
                    .zip(committed.container_info())
                    .is_some_and(|(current, committed)| {
                        current.container_id == committed.container_id
                            && current.workload_uid == committed.workload_uid
                    })
        }) {
            // A peer-sync may publish this exact committed generation first.
            // Keep any later metadata changes and acknowledge the same result.
            return Ok(());
        }
        ensure!(
            match (&baseline, &current) {
                (Some(before), Some(now)) => Arc::ptr_eq(before, now),
                (None, None) => true,
                _ => false,
            },
            "Builder registration changed while its replacement committed; retry the original operation"
        );
        let basic = committed
            .container_info()
            .context("Committed builder container missing")?;
        let identity = committed
            .persistence_identity()
            .container
            .clone()
            .context("Committed builder registration identity missing")?;
        self.inner
            .insert(committed.project_id().into(), Arc::new(committed))?;
        self.container_registration
            .lock()
            .map_err(|_| anyhow::anyhow!("Container registration lock poisoned"))?
            .insert(basic.container_name, identity);
        drop(registration);
        Ok(())
    }
}

pub(super) async fn rebind(
    tx: &mut dyn Executor,
    operation: &UserAppOperationRecord,
    evidence: &BuilderCreationEvidence,
    volumes: &[AppResourceIdentity],
) -> Result<ProjectAndContainerInfo> {
    // This root CAS is also the competition point for deletion and Stop/Restart.
    crate::userapp_lifecycle::bind_completed_builder_registration(tx, operation, evidence, volumes)
        .await?;
    rebind_rows(
        tx,
        &evidence.target.context,
        &evidence.target,
        &evidence.container,
        evidence.registration_predecessor.as_ref(),
        None,
    )
    .await
}

async fn rebind_rows(
    tx: &mut dyn Executor,
    context: &shared_types::UserAppExecutionContext,
    target: &shared_types::BuilderControlTarget,
    basic: &shared_types::ContainerBasicInfo,
    predecessor: Option<&shared_types::BuilderCreationPredecessor>,
    expected_registry: Option<&ProjectAndContainerInfo>,
) -> Result<ProjectAndContainerInfo> {
    let source = predecessor.and_then(|source| source.target.workload.as_ref());
    let replacement = target
        .workload
        .as_ref()
        .context("Builder replacement workload missing")?;
    ensure!(
        basic.project_id == context.app_id
            && basic.workload_uid.as_deref() == Some(replacement.uid.as_str())
            && target.pod.as_ref().is_some_and(
                |pod| pod.uid == basic.container_id && pod.name == basic.container_name
            ),
        "Builder registry identity differs from the captured replacement"
    );
    // Use the registry's existing lock namespace/order. These locks protect only
    // short SQL work, never a container or Kubernetes request.
    let mut keys = std::collections::BTreeSet::from([
        format!("container-name:{}", basic.container_name),
        format!("container:{}", basic.container_id),
        format!("project:{}", context.app_id),
    ]);
    if let Some(pod) = predecessor.and_then(|source| source.target.pod.as_ref()) {
        keys.insert(format!("container:{}", pod.uid));
    }
    if let Some(previous) = expected_registry.and_then(ProjectAndContainerInfo::container_info) {
        keys.insert(format!("container:{}", previous.container_id));
    }
    for key in keys {
        toasty::sql::query("SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1, 719324))")
            .bind(key)
            .exec(tx)
            .await?;
    }
    let previous = models::Container::filter_by_container_name(&basic.container_name)
        .first()
        .exec(tx)
        .await?
        .map(|row| {
            ensure!(
                row.service_type == ServiceType::UserappBuilder.to_string(),
                "Builder container is registered to another service family"
            );
            repo::ContainerRow::try_from(row)
        })
        .transpose()?;
    let project = models::Project::filter_by_project_id(&context.app_id)
        .first()
        .exec(tx)
        .await?;
    let mut old = match project {
        Some(project) => {
            ensure!(
                project.service_type.as_deref()
                    == Some(ServiceType::UserappBuilder.to_string().as_str()),
                "Builder registration belongs to another service family"
            );
            let fields = models::Session::fields();
            let sessions = models::Session::filter(
                fields
                    .project_id()
                    .eq(&project.project_id)
                    .and(fields.project_generation().eq(&project.generation)),
            )
            .exec(tx)
            .await?
            .into_iter()
            .map(repo::SessionRow::try_from)
            .collect::<Result<Vec<_>>>()?;
            let row = repo::ProjectRow::from_model(project, sessions)?;
            // hydrate_project takes owned rows; the same checked projection is
            // supplied below without reading another database snapshot.
            let container_map = previous
                .as_ref()
                .map(|c| {
                    let mut map = std::collections::HashMap::new();
                    map.insert(c.container_name.clone(), clone_container_row(c));
                    map
                })
                .unwrap_or_default();
            load::hydrate_project(&row, &container_map)?
        }
        None => {
            let mut info = ProjectAndContainerInfo::new(context.app_id.clone());
            info.set_service_type(Some(ServiceType::UserappBuilder));
            info
        }
    };
    if let Some(expected) = expected_registry {
        let expected_basic = expected
            .container_info()
            .context("Expected builder registration missing")?;
        let actual = previous
            .as_ref()
            .context("Old builder registration disappeared")?;
        let expected_container = expected
            .persistence_identity()
            .container
            .as_ref()
            .context("Expected builder persistence identity missing")?;
        ensure!(
            old.persistence_identity().generation == expected.persistence_identity().generation
                && old.persistence_identity().revision == expected.persistence_identity().revision
                && actual.container_generation == expected_container.generation
                && actual.row_revision == expected_container.revision
                && actual.container_id.as_deref() == Some(expected_basic.container_id.as_str())
                && actual.workload_uid == expected_basic.workload_uid,
            "Old builder registry changed during explicit adoption"
        );
    }
    if let Some(previous) = &previous {
        ensure!(
            previous.logical_id == context.app_id,
            "Builder container is registered to another project"
        );
        if previous.workload_uid.as_deref() == Some(replacement.uid.as_str()) {
            ensure!(
                previous.container_id.as_deref() == Some(basic.container_id.as_str()),
                "Completed builder Pod was replaced before registration retry"
            );
            ensure!(
                old.container_info()
                    .is_some_and(|info| info.container_id == basic.container_id),
                "Builder replacement registry reference is missing"
            );
            return Ok(old);
        }
        if let Some(source) = source {
            ensure!(
                previous.workload_uid.as_deref() == Some(source.uid.as_str()),
                "Builder predecessor workload differs from the current registry"
            );
        } else {
            let source_uid = previous
                .workload_uid
                .as_deref()
                .context("Legacy builder registry has no workload identity")?;
            let old = models::ResourceBinding::filter_by_service_type_and_physical_uid(
                ServiceType::UserappBuilder.to_string(),
                source_uid,
            )
            .first()
            .exec(tx)
            .await?
            .context("Legacy builder registry has no original lifecycle binding")?;
            ensure!(
                old.app_id == context.app_id && old.lifecycle_id == context.lifecycle_id,
                "Legacy builder registry belongs to another lifecycle"
            );
        }
        // A UserApp builder is dedicated to its app. Do not detach an unexpected
        // project's references while handling this app's completed operation.
        let rows = toasty::sql::query("SELECT project_id FROM projects WHERE container_name=$1 AND container_generation=$2 AND project_id<>$3")
            .bind(&previous.container_name).bind(&previous.container_generation).bind(&context.app_id).exec(tx).await?;
        ensure!(
            rows.is_empty(),
            "Builder registry has unrelated project references"
        );
    }
    let mut identity = old.persistence_identity().clone();
    identity.revision = identity
        .revision
        .checked_add(1)
        .context("Project revision exhausted")?;
    identity.container = Some(ContainerPersistenceIdentity {
        generation: crate::pg::uuid_generation(),
        revision: 1,
        physical_uid: Some(basic.container_id.clone()),
        predecessor: previous.as_ref().map(|c| c.container_generation.clone()),
        predecessor_revision: previous.as_ref().map(|c| c.row_revision),
    });
    old.set_container(Some(basic.clone()));
    old.set_persistence_identity(identity);
    let container = ContainerSnapshot::from_info(
        &basic.container_name,
        basic,
        &ServiceType::UserappBuilder,
        old.persistence_identity()
            .container
            .as_ref()
            .context("Builder snapshot has no container identity")?,
        basic.workload_uid.as_deref(),
    )?;
    ensure!(
        repo::upsert_container(tx, &container).await? == PersistenceOperationOutcome::Committed,
        "Builder registry predecessor changed during replacement"
    );
    ensure!(
        repo::upsert_project(tx, &ProjectSnapshot::from_info(&old)?).await?
            == PersistenceOperationOutcome::Committed,
        "Builder project changed during registry replacement"
    );
    Ok(old)
}

fn clone_container_row(row: &repo::ContainerRow) -> repo::ContainerRow {
    repo::ContainerRow {
        container_name: row.container_name.clone(),
        container_generation: row.container_generation.clone(),
        row_revision: row.row_revision,
        container_id: row.container_id.clone(),
        workload_uid: row.workload_uid.clone(),
        logical_id: row.logical_id.clone(),
        container_ip: row.container_ip.clone(),
        internal_port: row.internal_port,
        external_port: row.external_port,
        status: row.status.clone(),
        service_url: row.service_url.clone(),
        created_at: row.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::*;
    use std::time::Duration;

    #[tokio::test]
    async fn verified_builder_replacement_atomically_rebinds_registry_and_preserves_sessions() {
        let Some(dsn) = crate::pg::test_support::test_dsn().await else {
            eprintln!("[skip] explicit isolated PostgreSQL DSN required");
            return;
        };
        // Registration joins the project and lifecycle domains in one real
        // transaction. A fresh disposable database needs both production
        // migration components, rather than the project-only test helper.
        let owner = crate::db::postgres::open(
            &crate::config::PostgresConfig {
                url: Some(dsn.clone()),
                ..Default::default()
            },
            vec![
                crate::db::schema::Component::Project,
                crate::db::schema::Component::Userapp,
            ],
        )
        .await
        .expect("disposable PG project and lifecycle database");
        for (legacy, explicit_adoption) in [(false, false), (true, false), (true, true)] {
            let control = crate::userapp_lifecycle::PgUserAppStore::from_owner(
                owner.clone(),
                crate::db::schema::Backend::Postgres,
            );
            let app_id = format!("builderreplace{}", uuid::Uuid::new_v4().simple());
            let app = control.ensure_identity(&app_id).await.unwrap();
            let accepted = match control
                .admit(&UserAppAdmission {
                    app_id: app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    operation_id: format!("ensure{}", uuid::Uuid::new_v4().simple()),
                    request_id: None,
                    request_fingerprint: "a".repeat(64),
                    kind: UserAppOperationKind::EnsureBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap()
            {
                UserAppAdmissionOutcome::Accepted(record) => record,
                _ => panic!("fresh admission"),
            };
            let running = control
                .advance(&UserAppOperationProgress {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: accepted.operation_id.clone(),
                    expected_revision: accepted.revision,
                    executor_id: "replacementworker".into(),
                    state: UserAppOperationState::Running,
                    step: "claimed".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
            let context = UserAppExecutionContext {
                app_id: app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                executor_id: "replacementworker".into(),
                request_fingerprint: running.request_fingerprint.clone(),
            };
            let old_uid = format!("old{}", uuid::Uuid::new_v4().simple());
            let new_uid = format!("new{}", uuid::Uuid::new_v4().simple());
            if legacy {
                // The historical source binding is preserved independently of the
                // current EnsureBuilder. Its FK uses this existing operation; the
                // binding lookup below still checks exact app/lifecycle/old UID.
                let old_uid = old_uid.clone();
                let app_id = app_id.clone();
                let life = app.lifecycle_id.clone();
                let operation_id = running.operation_id.clone();
                owner.execute(move |mut db| async move {
                toasty::sql::statement("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6)")
                    .bind(ServiceType::UserappBuilder.to_string()).bind(old_uid).bind(app_id).bind(life).bind(operation_id)
                    .bind(chrono::Utc::now().timestamp_micros()).exec(&mut db).await?;
                Ok(())
            }).await.unwrap();
            }
            let target = |uid: &str, pod_uid: &str| BuilderControlTarget {
                resource_binding: None,
                context: context.clone(),
                workload: Some(AppResourceIdentity {
                    kind: AppResourceKind::StatefulSet,
                    name: format!("builder-{app_id}"),
                    uid: uid.into(),
                    resource_version: Some("1".into()),
                }),
                pod: Some(BuilderPodIdentity {
                    name: format!("builder-{app_id}-0"),
                    uid: pod_uid.into(),
                    resource_version: "1".into(),
                }),
                restart_image: None,
            };
            let pod_before = format!("before{}", uuid::Uuid::new_v4().simple());
            let pod_after = format!("after{}", uuid::Uuid::new_v4().simple());
            let basic = |pod_uid: &str, workload_uid: &str| ContainerBasicInfo {
                container_id: pod_uid.into(),
                container_name: format!("builder-{app_id}-0"),
                container_ip: "10.42.0.9".into(),
                internal_port: 60000,
                external_port: 0,
                project_id: app_id.clone(),
                status: "running".into(),
                created_at: chrono::Utc::now(),
                service_url: "http://10.42.0.9:60000".into(),
                workload_uid: Some(workload_uid.into()),
            };
            let (store, _cleanup) = PgStore::connect(
                &crate::config::PostgresConfig {
                    url: Some(dsn.clone()),
                    ..Default::default()
                },
                "builder-replacement-test".into(),
                "cluster.local".into(),
            )
            .await
            .unwrap();
            let session = format!("session{app_id}");
            let mut registered = ProjectAndContainerInfo::new(app_id.clone());
            registered.set_service_type(Some(ServiceType::UserappBuilder));
            registered.set_container(Some(basic(&pod_before, &old_uid)));
            store
                .insert_with_session(app_id.clone(), Arc::new(registered), Some(&session))
                .unwrap();
            assert!(store.wait_drained(Duration::from_secs(5)).await);
            let original = store.get(&app_id).unwrap();
            let original_generation = original.persistence_identity().generation.clone();
            let original_container_generation = original
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .generation
                .clone();
            let volumes = vec![AppResourceIdentity {
                kind: AppResourceKind::PersistentVolumeClaim,
                name: format!("workspace-{app_id}"),
                uid: format!("pvc{app_id}"),
                resource_version: None,
            }];
            let mut evidence = BuilderCreationEvidence {
                creation_lease_released: true,
                target: target(&new_uid, &pod_after),
                container: basic(&pod_after, &new_uid),
                registration_predecessor: Some(BuilderCreationPredecessor {
                    target: target(&old_uid, &pod_before),
                    volumes: volumes.clone(),
                }),
            };
            if legacy {
                evidence.registration_predecessor = None;
                evidence.target.resource_binding = Some(UserAppResourceBinding {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: new_uid.clone(),
                    adopted_by_operation: running.operation_id.clone(),
                });
            }
            let ready = control
                .advance(&UserAppOperationProgress {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    expected_revision: running.revision,
                    executor_id: context.executor_id.clone(),
                    state: UserAppOperationState::Running,
                    step: "builder_ready_confirmed".into(),
                    checkpoint: serde_json::to_value(&evidence).unwrap(),
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
            let mut checkpoint = serde_json::to_value(&evidence.container).unwrap();
            if !legacy {
                checkpoint.as_object_mut().unwrap().insert(
                    "builder_creation_evidence".into(),
                    serde_json::to_value(&evidence).unwrap(),
                );
            }
            let completed = control
                .advance(&UserAppOperationProgress {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: ready.operation_id.clone(),
                    expected_revision: ready.revision,
                    executor_id: context.executor_id.clone(),
                    state: UserAppOperationState::Succeeded,
                    step: "creation_result".into(),
                    checkpoint,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
            // This is the original failure: general registration still refuses a
            // different workload UID, even though physical creation succeeded.
            let mut ordinary = (*original).clone();
            ordinary.set_container(Some(evidence.container.clone()));
            assert!(store.insert(app_id.clone(), Arc::new(ordinary)).is_err());
            if explicit_adoption {
                // Pre-receipt workloads have no private creator acknowledgement.
                // Their explicit Adopt request grants registration only, and the
                // historical Succeeded Ensure remains byte-for-byte unchanged.
                let request = AdoptBuilderRequest {
                    lifecycle_id: app.lifecycle_id.clone(),
                    request_id: format!("adopt{}", uuid::Uuid::new_v4().simple()),
                    expected_container_id: pod_after.clone(),
                };
                let input = UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
                let accepted = match control
                    .admit_with_input(
                        &UserAppAdmission {
                            app_id: app_id.clone(),
                            lifecycle_id: Some(app.lifecycle_id.clone()),
                            operation_id: format!("adoptop{}", uuid::Uuid::new_v4().simple()),
                            request_id: Some(request.request_id.clone()),
                            request_fingerprint: input.digest(),
                            kind: UserAppOperationKind::AdoptBuilder,
                            command: None,
                            metadata: None,
                            runtime_policy_on_success: None,
                        },
                        Some(&input),
                    )
                    .await
                    .unwrap()
                {
                    UserAppAdmissionOutcome::Accepted(record) => record,
                    _ => panic!("fresh adoption"),
                };
                let running_adoption = control
                    .advance(&UserAppOperationProgress {
                        app_id: app_id.clone(),
                        lifecycle_id: app.lifecycle_id.clone(),
                        operation_id: accepted.operation_id.clone(),
                        expected_revision: accepted.revision,
                        executor_id: "adoptionworker".into(),
                        state: UserAppOperationState::Running,
                        step: "claimed".into(),
                        checkpoint: serde_json::Value::Null,
                        error_code: None,
                        error_message: None,
                    })
                    .await
                    .unwrap();
                let mut adopted = evidence.target.clone();
                adopted.context = UserAppExecutionContext {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: running_adoption.operation_id.clone(),
                    executor_id: "adoptionworker".into(),
                    request_fingerprint: running_adoption.request_fingerprint.clone(),
                };
                // Already-bound current resources retain that binding's original
                // operation identity instead of being rewritten by the repair.
                let existing_binding = evidence.target.resource_binding.clone().unwrap();
                let binding = existing_binding.clone();
                owner.execute(move |mut db| async move {
                    toasty::sql::statement("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6)")
                        .bind(binding.service_type.to_string()).bind(binding.physical_uid).bind(binding.app_id)
                        .bind(binding.lifecycle_id).bind(binding.adopted_by_operation)
                        .bind(chrono::Utc::now().timestamp_micros()).exec(&mut db).await?;
                    Ok(())
                }).await.unwrap();
                adopted.resource_binding = Some(existing_binding.clone());
                let progress = UserAppOperationProgress {
                    app_id: app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: running_adoption.operation_id.clone(),
                    expected_revision: running_adoption.revision,
                    executor_id: "adoptionworker".into(),
                    state: UserAppOperationState::Succeeded,
                    step: "physical_resource_adopted".into(),
                    checkpoint: serde_json::json!({
                        "operation_id": running_adoption.operation_id, "was_existing": true,
                        "container": evidence.container,
                        "registration_target": adopted, "registration_volumes": volumes,
                    }),
                    error_code: None,
                    error_message: None,
                };
                let mut stale = (*original).clone();
                let mut identity = stale.persistence_identity().clone();
                identity.revision += 1;
                stale.set_persistence_identity(identity);
                assert!(
                    store
                        .complete_builder_registration_adoption(
                            &stale,
                            &adopted,
                            &evidence.container,
                            &volumes,
                            &progress
                        )
                        .await
                        .is_err()
                );
                assert_eq!(
                    control
                        .get_operation(&app_id, &running_adoption.operation_id)
                        .await
                        .unwrap()
                        .unwrap(),
                    running_adoption,
                    "A stale registry CAS must roll back the adoption terminal and binding changes"
                );
                let mut foreign = adopted.clone();
                foreign.context.lifecycle_id = "foreignlife".into();
                assert!(
                    store
                        .complete_builder_registration_adoption(
                            &original,
                            &foreign,
                            &evidence.container,
                            &volumes,
                            &progress
                        )
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .complete_builder_registration_adoption(
                            &original,
                            &adopted,
                            &evidence.container,
                            &[],
                            &progress
                        )
                        .await
                        .is_err()
                );
                store
                    .complete_builder_registration_adoption(
                        &original,
                        &adopted,
                        &evidence.container,
                        &volumes,
                        &progress,
                    )
                    .await
                    .unwrap();
                let repaired = store.get(&app_id).unwrap();
                assert_eq!(repaired.container_info().unwrap().container_id, pod_after);
                assert_eq!(
                    repaired.persistence_identity().generation,
                    original_generation
                );
                assert!(repaired.sessions().contains(&session));
                assert_eq!(
                    control
                        .get_operation(&app_id, &completed.operation_id)
                        .await
                        .unwrap()
                        .unwrap(),
                    completed,
                    "Explicit repair cannot forge or rewrite the old creation outcome"
                );
                assert_eq!(
                    control
                        .get_resource_binding(&ServiceType::UserappBuilder, &new_uid)
                        .await
                        .unwrap()
                        .unwrap(),
                    existing_binding
                );
                assert_eq!(
                    control
                        .get_operation(&app_id, &running_adoption.operation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .state,
                    UserAppOperationState::Succeeded
                );
                assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
                continue;
            }
            let mut changed_volume = volumes.clone();
            if legacy {
                changed_volume[0].kind = AppResourceKind::Secret;
            } else {
                changed_volume[0].uid = "foreignvolume".into();
            }
            assert!(
                store
                    .register_completed_builder_replacement(&completed, &evidence, &changed_volume)
                    .await
                    .is_err()
            );
            assert!(
                control
                    .get_resource_binding(&ServiceType::UserappBuilder, &new_uid)
                    .await
                    .unwrap()
                    .is_none(),
                "Rejected replacement must not commit half of the binding"
            );
            store
                .register_completed_builder_replacement(&completed, &evidence, &volumes)
                .await
                .unwrap();
            let rebound = store.get(&app_id).unwrap();
            assert_eq!(rebound.container_info().unwrap().container_id, pod_after);
            assert_eq!(
                rebound.persistence_identity().generation,
                original_generation
            );
            assert!(rebound.sessions().contains(&session));
            assert_eq!(
                store
                    .get_by_session_id(&session)
                    .unwrap()
                    .container_info()
                    .unwrap()
                    .workload_uid
                    .as_deref(),
                Some(new_uid.as_str())
            );
            assert_eq!(
                rebound
                    .persistence_identity()
                    .container
                    .as_ref()
                    .unwrap()
                    .predecessor
                    .as_deref(),
                Some(original_container_generation.as_str())
            );
            let binding = control
                .get_resource_binding(&ServiceType::UserappBuilder, &new_uid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(binding.lifecycle_id, app.lifecycle_id);
            assert_eq!(binding.adopted_by_operation, completed.operation_id);
            let installed = rebound
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .generation
                .clone();
            // Same original operation is replayable and does not manufacture another
            // registry generation after an uncertain HTTP response.
            store
                .register_completed_builder_replacement(&completed, &evidence, &volumes)
                .await
                .unwrap();
            assert_eq!(
                store
                    .get(&app_id)
                    .unwrap()
                    .persistence_identity()
                    .container
                    .as_ref()
                    .unwrap()
                    .generation,
                installed
            );
            assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
        }
    }
}

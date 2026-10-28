//! Actual Docker CID/volume evidence for metadata-only retirement.
//! No RCoder service, production runtime, or shared container is restarted.
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeResult, ContainerRuntimeStatus,
    RemovedContainerInfo, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn docker(args: &[&str]) -> String {
    let result = Command::new("docker").args(args).output().unwrap();
    assert!(
        result.status.success(),
        "Docker fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}
struct Owned {
    run: String,
    name: String,
    volume: String,
    ids: Vec<String>,
}
impl Owned {
    fn create(&mut self, image: &str) -> String {
        let id = docker(&[
            "create",
            "--name",
            &self.name,
            "--label",
            &format!("rcoder.e2e.run={}", self.run),
            "--volume",
            &format!("{}:/evidence", self.volume),
            "--entrypoint",
            "sh",
            image,
            "-c",
            "exec sleep 300",
        ]);
        assert_eq!(id.len(), 64);
        self.ids.push(id.clone());
        self.verify(&id);
        docker(&["start", &id]);
        id
    }
    fn verify(&self, id: &str) {
        assert_eq!(
            docker(&[
                "inspect",
                "--format",
                "{{.Id}} {{index .Config.Labels \"rcoder.e2e.run\"}}",
                id
            ]),
            format!("{id} {}", self.run)
        );
    }
    fn remove(&self, id: &str) {
        self.verify(id);
        docker(&["stop", "--time", "2", id]);
        docker(&["rm", id]);
    }
    fn witness(&self, id: &str, marker: &str) {
        self.verify(id);
        assert_eq!(
            docker(&["inspect", "--format", "{{.Id}} {{.State.Running}}", id]),
            format!("{id} true")
        );
        assert_eq!(docker(&["exec", id, "cat", "/evidence/sentinel"]), marker);
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        for id in &self.ids {
            let observed = Command::new("docker")
                .args([
                    "inspect",
                    "--format",
                    "{{.Id}} {{index .Config.Labels \"rcoder.e2e.run\"}}",
                    id,
                ])
                .output();
            if let Ok(row) = observed
                && row.status.success()
                && String::from_utf8_lossy(&row.stdout).trim() == format!("{id} {}", self.run)
            {
                let _removed = Command::new("docker").args(["rm", "--force", id]).output();
            }
        }
        // Never -v / volume rm: preserve the actual data witness even on failure.
    }
}
fn registration(project: &str, user: &str, name: &str, id: &str) -> Arc<ProjectAndContainerInfo> {
    let mut value = ProjectAndContainerInfo::new(project.into());
    value.set_user_id(Some(user.into()));
    value.set_service_type(Some(ServiceType::ComputerAgentRunner));
    value.set_container(Some(ContainerBasicInfo {
        container_id: id.into(),
        container_name: name.into(),
        container_ip: "127.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: user.into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://127.0.0.1:50051".into(),
        workload_uid: Some(id.into()),
    }));
    Arc::new(value)
}

/// A real old Reaper can reach this logical-delete method. It deliberately
/// resolves the current same-name CID, reproducing the original hazard, but
/// can operate only on this run's exact name, logical ID and ownership label.
struct Runtime {
    run: String,
    name: String,
    logical: String,
    physical_delete_calls: AtomicUsize,
    sync_calls: AtomicUsize,
}
#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("metadata test never creates through business runtime")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        assert_eq!(identifier, self.logical);
        assert!(service_type.is_computer_family());
        let raw = docker(&["inspect", "--format", "{{json .}}", &self.name]);
        let row: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            row["Config"]["Labels"]["rcoder.e2e.run"].as_str(),
            Some(self.run.as_str())
        );
        Ok(Some(RuntimeContainerInfo {
            container_id: row["Id"].as_str().unwrap().into(),
            container_name: self.name.clone(),
            container_ip: "127.0.0.1".into(),
            status: ContainerRuntimeStatus::Running,
            created_at: chrono::Utc::now(),
            env_vars: None,
            service_type: Some(*service_type),
            project_id: None,
            user_id: Some(identifier.into()),
            pod_id: None,
            app_id: None,
            workload_uid: None,
        }))
    }
    async fn stop_container(&self, identifier: &str) -> ContainerRuntimeResult<()> {
        assert_eq!(identifier, self.logical);
        let observed = docker(&[
            "inspect",
            "--format",
            "{{.Id}} {{index .Config.Labels \"rcoder.e2e.run\"}}",
            &self.name,
        ]);
        let (id, label) = observed.split_once(' ').unwrap();
        assert_eq!(label, self.run);
        assert_eq!(id.len(), 64);
        self.physical_delete_calls.fetch_add(1, Ordering::SeqCst);
        docker(&["rm", "--force", id]);
        Ok(())
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(true)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("cannot clean other applications")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        Ok((0, vec![]))
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
#[ignore = "requires explicit local Docker fixture image; use leader_pg_fixture.py"]
async fn actual_docker_old_metadata_drop_preserves_same_name_new_cid_and_volume() {
    let image =
        std::env::var("RCODER_DOCKER_FAULT_IMAGE").expect("explicit immutable local fixture image");
    assert!(image.starts_with("sha256:") && image.len() == 71);
    assert_eq!(
        docker(&["image", "inspect", "--format", "{{.Id}}", &image]),
        image
    );
    let run = uuid::Uuid::new_v4().simple().to_string();
    let volume = format!("rcoder-metadata-data-{run}");
    docker(&[
        "volume",
        "create",
        "--label",
        &format!("rcoder.e2e.run={run}"),
        &volume,
    ]);
    let mut owned = Owned {
        run: run.clone(),
        name: format!("rcoder-metadata-{}", &run[..16]),
        volume: volume.clone(),
        ids: Vec::new(),
    };
    let marker = format!("retained-data-{run}");
    let old = owned.create(&image);
    docker(&[
        "exec",
        &old,
        "sh",
        "-ec",
        "printf '%s' \"$1\" > /evidence/sentinel; sync",
        "--",
        &marker,
    ]);
    let retired = Arc::new(rcoder_storage::ProjectAdapter::new(
        "owned-metadata".into(),
        "cluster.local".into(),
    ));
    let old_project = format!("old-{run}");
    let user = format!("user-{run}");
    retired
        .insert(
            old_project.clone(),
            registration(&old_project, &user, &owned.name, &old),
        )
        .unwrap();
    let delayed_snapshot = retired.get(&old_project).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let userapp = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &temporary.path().join("userapp.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(Runtime {
        run: run.clone(),
        name: owned.name.clone(),
        logical: user.clone(),
        physical_delete_calls: AtomicUsize::new(0),
        sync_calls: AtomicUsize::new(0),
    });
    let mut state =
        crate::userapp_builder::builder_runtime_test_state(runtime.clone(), userapp.clone(), None)
            .await;
    let unique = Arc::get_mut(&mut state).unwrap();
    unique.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Memory(retired.clone()));
    unique.config.cleanup_config.enabled = true;
    unique.config.cleanup_config.cleanup_interval_seconds = 86400;
    unique.config.cleanup_config.idle_timeout_seconds = 86400;
    unique.config.cleanup_config.container_protection_seconds = 86400;
    unique.config.cleanup_config.log_cleanup.log_dir =
        temporary.path().join("logs").to_string_lossy().into_owned();
    unique.config.userapp_recycle.enabled = false;
    let (shutdown, _) = tokio::sync::broadcast::channel(4);
    let handles = super::start_all_background_tasks(&state.config, state.clone(), shutdown.clone())
        .await
        .unwrap();
    assert!(
        handles.cleanup_handle.is_some(),
        "real cleanup/background consumer must be assembled"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.sync_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    owned.remove(&old);
    let replacement = owned.create(&image);
    assert_ne!(old, replacement);
    let current =
        rcoder_storage::ProjectAdapter::new("owned-metadata".into(), "cluster.local".into());
    let new_project = format!("new-{run}");
    let session = format!("session-{run}");
    current
        .insert_with_session(
            new_project.clone(),
            registration(&new_project, &user, &owned.name, &replacement),
            Some(&session),
        )
        .unwrap();
    owned.witness(&replacement, &marker);
    // The old metadata is deliberately consumed only after B owns the same
    // logical name, reproducing the dangerous delayed-retirement ordering.
    drop(retired.remove(&old_project));
    retired.delete_container_with_projects(&old);
    drop(delayed_snapshot);
    drop(retired);
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        runtime.physical_delete_calls.load(Ordering::SeqCst),
        0,
        "metadata retirement must not enqueue physical deletion against the replacement CID"
    );
    owned.witness(&replacement, &marker);
    let routed = current.get_by_session_id(&session).unwrap();
    assert_eq!(routed.project_id(), new_project);
    assert_eq!(routed.container_info().unwrap().container_id, replacement);
    assert_eq!(
        routed.container_info().unwrap().workload_uid.as_deref(),
        Some(replacement.as_str())
    );
    let _ = shutdown.send(());
    handles
        .drain(tokio::time::Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    owned.witness(&replacement, &marker);
    userapp.shutdown().await.unwrap();
    if let Some(path) = std::env::var_os("RCODER_DOCKER_FAULT_REPORT") {
        let report = serde_json::json!({"success":true,"scope":"actual_docker_with_real_background_assembly_and_controlled_owned_logical_delete_runtime_not_full_http_routing","old_cid":old,"replacement_cid":replacement,"same_name":owned.name,"replacement_running":true,"replacement_session_lookup_preserved":true,"sentinel_retained":true,"physical_delete_calls":runtime.physical_delete_calls.load(Ordering::SeqCst),"sync_calls":runtime.sync_calls.load(Ordering::SeqCst),"retained_volume":volume});
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}

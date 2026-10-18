//! Legal K8s PVC identities, stateful resources and real data-directory purge.
//! Runtime/API protocol fixture, not a real-cluster acceptance claim.
use super::*;
use crate::models::{
    ClearStorageRequest, DeleteAppRequest, DestroyStorageRequest, StartAppRequest,
};
use crate::test_support::StubDevCleanup;
use container_runtime_api::WorkspaceRuntime;

#[derive(Clone)]
struct PvcDevCleanup {
    runtime: Arc<MockRuntime>,
    app_id: String,
    captured: Vec<(AppResourceIdentity, std::path::PathBuf)>,
}

struct PvcDevDeletion {
    cleanup: PvcDevCleanup,
    lease: Option<Box<dyn AppOperationLease>>,
}

#[async_trait::async_trait]
impl UserappDevCleanup for PvcDevCleanup {
    async fn capture(&self, app_id: &str) -> Result<Box<dyn UserappDevDeletion>, String> {
        if app_id != self.app_id {
            return Err("foreign fixture app".into());
        }
        Ok(Box::new(PvcDevDeletion {
            cleanup: self.clone(),
            lease: None,
        }))
    }
    async fn capture_with_lease(
        &self,
        app_id: &str,
        lease: Box<dyn AppOperationLease>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        if app_id != self.app_id {
            return Err("foreign fixture app".into());
        }
        Ok(Box::new(PvcDevDeletion {
            cleanup: self.clone(),
            lease: Some(lease),
        }))
    }
}

#[async_trait::async_trait]
impl UserappDevDeletion for PvcDevDeletion {
    fn receipt(&self) -> UserappDevDeletionReceipt {
        UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                app_id: self.cleanup.app_id.clone(),
                operation_id: "captured-dev-pvcs".into(),
                resources: self
                    .cleanup
                    .captured
                    .iter()
                    .map(|(resource, _)| resource.clone())
                    .collect(),
                docker_bind_cleanup: false,
                resource_binding: None,
            },
            registry: None,
            lease: None,
            collaborators: Vec::new(),
            directories: None,
        }
    }
    async fn cleanup(self: Box<Self>) -> Result<(), String> {
        for (resource, path) in &self.cleanup.captured {
            if self
                .cleanup
                .runtime
                .physical_pvc_uids
                .get(&resource.name)
                .as_deref()
                != Some(&resource.uid)
            {
                return Err("captured dev PVC was replaced".into());
            }
            self.cleanup
                .runtime
                .physical_pvc_uids
                .remove(&resource.name);
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(lease) = self.lease {
            lease.release().await?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn discovered_dev_storage_destroyed_by_original_operation_allows_dev_ensure_verification() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let app_id = "discovereddevdestroy";
    let path = directory.path().join("dev-pvc-data");
    tokio::fs::create_dir_all(&path).await.unwrap();
    tokio::fs::write(path.join("original-data"), b"original dev data")
        .await
        .unwrap();
    let resource = AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "original-dev-pvc".into(),
        uid: "original-dev-pvc-uid".into(),
        resource_version: Some("1".into()),
    };
    let captured = vec![(resource.clone(), path.clone())];
    runtime
        .physical_pvc_uids
        .insert(resource.name.clone(), resource.uid.clone());
    runtime
        .recovered_pvc_volumes
        .insert(app_id.into(), captured.clone());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    service
        .set_dev_cleanup(Arc::new(PvcDevCleanup {
            runtime: runtime.clone(),
            app_id: app_id.into(),
            captured,
        }))
        .unwrap();
    let mut found = discovered(app_id, "discovered-dev-lifecycle");
    found.dev_volumes = vec![resource.clone()];
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .extend([Ok(Some(found.clone())), Ok(Some(found.clone()))]);
    let identity = service
        .discover_missing_identity(app_id)
        .await
        .unwrap()
        .unwrap();
    service
        .verify_recovered_storage(app_id, UserAppOperationScope::Dev)
        .await
        .unwrap();
    let operation_id = service
        .destroy_app_storage_controlled(
            UserappStage::Dev,
            app_id,
            DestroyStorageRequest {
                confirm: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                request_id: Some("intentional-dev-destroy".into()),
            },
        )
        .await
        .unwrap();
    assert!(!path.exists() && !runtime.physical_pvc_uids.contains_key(&resource.name));
    let original = store
        .get_operation(app_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.kind, UserAppOperationKind::DestroyDevStorage);
    assert_eq!(original.state, UserAppOperationState::Succeeded);
    assert_eq!(
        store
            .get_application(app_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_id,
        identity.lifecycle_id
    );
    // This is the same verification entry invoked by normal builder ensure.
    service
        .verify_recovered_storage(app_id, UserAppOperationScope::Dev)
        .await
        .unwrap();
    runtime
        .ensure_workspace(app_id, &ServiceType::UserappBuilder, None)
        .await
        .unwrap();
    assert!(path.is_dir() && !path.join("original-data").exists());
    assert!(
        runtime
            .physical_pvc_uids
            .get(&resource.name)
            .is_some_and(|uid| *uid != resource.uid)
    );
    assert_eq!(
        store
            .get_recovery_witness(app_id, &identity.lifecycle_id)
            .await
            .unwrap(),
        Some(found)
    );
    assert_eq!(
        store.get_operation(app_id, &operation_id).await.unwrap(),
        Some(original)
    );
    store.shutdown().await.unwrap();
}

#[derive(Clone, Copy)]
enum Removal {
    Purge,
    DestroyProd,
    ClearProd,
}

#[tokio::test]
async fn discovered_storage_purged_by_original_operation_does_not_block_requested_new_artifact() {
    requested_new_artifact_after(Removal::Purge).await;
}

#[tokio::test]
async fn discovered_storage_destroyed_by_original_operation_does_not_block_requested_new_artifact()
{
    requested_new_artifact_after(Removal::DestroyProd).await;
}

#[tokio::test]
async fn discovered_storage_cleared_by_original_k8s_operation_does_not_block_requested_new_artifact()
 {
    requested_new_artifact_after(Removal::ClearProd).await;
}

async fn requested_new_artifact_after(removal: Removal) {
    crate::test_support::restart_image_env();
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let app_id = "discoveredpurge";
    let captured: Vec<_> = ["workspace", "data"]
        .into_iter()
        .map(|name| {
            let path = directory.path().join("original-prod").join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("original-data"), b"must be purged intentionally").unwrap();
            (
                AppResourceIdentity {
                    kind: AppResourceKind::PersistentVolumeClaim,
                    name: format!("original-{name}-pvc"),
                    uid: format!("original-{name}-pvc-uid"),
                    resource_version: Some("1".into()),
                },
                path,
            )
        })
        .collect();
    let volumes: Vec<_> = captured.iter().map(|(volume, _)| volume.clone()).collect();
    for volume in &volumes {
        runtime
            .physical_pvc_uids
            .insert(volume.name.clone(), volume.uid.clone());
    }
    runtime
        .recovered_pvc_volumes
        .insert(app_id.into(), captured.clone());
    let (mut service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    service
        .set_dev_cleanup(Arc::new(StubDevCleanup::default()))
        .unwrap();
    let mut found = discovered(app_id, "discovered-lifecycle");
    found.prod_uid = Some("old-prod-container".into());
    found.prod_volumes = volumes.clone();
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .extend([Ok(Some(found.clone())), Ok(Some(found.clone()))]);
    let identity = service
        .discover_missing_identity(app_id)
        .await
        .unwrap()
        .unwrap();
    service
        .verify_recovered_storage(app_id, UserAppOperationScope::Prod)
        .await
        .unwrap();
    let witness = store
        .get_recovery_witness(app_id, &identity.lifecycle_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(witness, found);
    runtime.deployments.insert(
        app_id.into(),
        container_runtime_api::DeploymentStatus {
            app_id: app_id.into(),
            lifecycle_id: Some(identity.lifecycle_id.clone()),
            phase: "Running".into(),
            replicas: 1,
            ready_replicas: 1,
            ..Default::default()
        },
    );
    let request_id = "intentional-storage-removal";
    let kind = match removal {
        Removal::Purge => {
            service
                .delete_app_controlled(
                    app_id,
                    DeleteAppRequest {
                        purge: Some(true),
                        lifecycle_id: Some(identity.lifecycle_id.clone()),
                        request_id: Some(request_id.into()),
                        expected_resource_version: None,
                    },
                )
                .await
                .unwrap();
            UserAppOperationKind::PurgeResources
        }
        Removal::DestroyProd => {
            runtime.deployments.remove(app_id);
            service
                .destroy_app_storage_controlled(
                    UserappStage::Prod,
                    app_id,
                    DestroyStorageRequest {
                        confirm: app_id.into(),
                        lifecycle_id: Some(identity.lifecycle_id.clone()),
                        request_id: Some(request_id.into()),
                    },
                )
                .await
                .unwrap();
            UserAppOperationKind::DestroyProdStorage
        }
        Removal::ClearProd => {
            runtime.deployments.remove(app_id);
            // K8s clear destroys the captured PVC. Docker clear only clears
            // directory contents and must never retire a preserved volume.
            service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
            service
                .clear_app_storage_controlled(
                    UserappStage::Prod,
                    app_id,
                    ClearStorageRequest {
                        lifecycle_id: Some(identity.lifecycle_id.clone()),
                        request_id: Some(request_id.into()),
                    },
                )
                .await
                .unwrap();
            service.config.access_mode = crate::config::AppAccessMode::Docker;
            UserAppOperationKind::ClearProdStorage
        }
    };
    let retained = store.get_application(app_id).await.unwrap().unwrap();
    assert_eq!(retained.state, UserAppLifecycleState::Active);
    assert_eq!(
        retained.lifecycle_id, identity.lifecycle_id,
        "resource removal does not replace the lifecycle"
    );
    assert!(captured.iter().all(|(_, path)| !path.exists()));
    assert!(!runtime.deployments.contains_key(app_id));
    let purge = store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(purge.kind, kind);
    assert_eq!(purge.state, UserAppOperationState::Succeeded);
    let original_purge = serde_json::to_value(&purge).unwrap();
    let original_witness = serde_json::to_value(&witness).unwrap();

    // The app-cli protocol fixture reports the current operation captured by
    // the original create path. No model or business HTTP response is faked.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", APP_CLI_ADMIN_PORT))
        .await
        .unwrap();
    let observed_runtime = runtime.clone();
    let router = axum::Router::new().route("/v1/deploy/status", axum::routing::get(move || {
        let runtime = observed_runtime.clone();
        async move {
            let params = runtime.native_params.get(app_id).unwrap().clone();
            let op = params.execution_context.unwrap().operation_id;
            axum::Json(serde_json::json!({"data": {
                "protocol_version": APP_CLI_UNIFIED_DEPLOY_PROTOCOL,
                "phase":AppCliDeployPhase::Running, "operation": {
                    "operation_id":op, "deployment_generation_id":op,
                    "request_release_id":"fresh-request", "artifact_release_id":"fresh-artifact",
                    "deploy_stage":AppDeploymentStage::Succeeded, "persisted":true, "phase":AppCliDeployPhase::Running, "error":null
                }
            }}))
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        service.start_app_enhanced(
            app_id,
            StartAppRequest {
                url: Some("https://artifact.invalid/fresh.zip".into()),
                release_id: Some("fresh-request".into()),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                request_id: Some("new-artifact-after-purge".into()),
                auto_execute_sql: Some(false),
                ..Default::default()
            },
        ),
    )
    .await;
    server.abort();
    let result = result.expect("normal deployment must not wait on obsolete storage");
    assert!(
        result.is_ok(),
        "confirmed original purge must permit requested new artifact: {result:?}"
    );
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.deployments.len(), 1);
    for volume in &volumes {
        assert!(
            runtime
                .physical_pvc_uids
                .get(&volume.name)
                .is_some_and(|current| *current != volume.uid),
            "fresh creation has a new PVC UID instead of adopting the retired witness"
        );
    }
    assert!(captured.iter().all(|(_, path)| path.is_dir()));
    assert!(
        captured
            .iter()
            .all(|(_, path)| !path.join("original-data").exists())
    );
    assert_eq!(
        serde_json::to_value(
            store
                .get_recovery_witness(app_id, &identity.lifecycle_id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        original_witness
    );
    assert_eq!(
        serde_json::to_value(
            store
                .get_operation(app_id, &purge.operation_id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        original_purge
    );
    store.shutdown().await.unwrap();
}

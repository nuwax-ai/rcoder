use super::*;
use container_runtime_api::ContainerRuntimeResult;
use shared_types::{AppResourceIdentity, AppResourceKind, UserAppExecutionContext};
use std::sync::Mutex;

#[derive(Default)]
struct Runtime {
    plain: Mutex<Vec<BuilderControlTarget>>,
    archived: Mutex<Vec<(BuilderControlTarget, BuilderRestartTemplate)>>,
    stopped: bool,
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn reconcile_builder_compute_stop(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self.stopped)
    }
    async fn create_container(
        &self,
        _: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("restart dispatch must not create a container")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("unexpected lookup")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &shared_types::ServiceType,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
        panic!("unexpected lookup")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("unexpected stop")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("unexpected probe")
    }
    async fn list_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
        panic!("unexpected list")
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("unexpected cleanup")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        panic!("unexpected health check")
    }
    async fn start_builder_control(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.plain.lock().expect("plain calls").push(target.clone());
        Ok(None)
    }

    async fn start_builder_control_with_restart(
        &self,
        target: &BuilderControlTarget,
        template: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.archived
            .lock()
            .expect("archived calls")
            .push((target.clone(), template.clone()));
        Ok(None)
    }
}

fn target() -> BuilderControlTarget {
    BuilderControlTarget {
        resource_binding: None,
        context: UserAppExecutionContext {
            app_id: "builder1".into(),
            lifecycle_id: "life1".into(),
            operation_id: "restart1".into(),
            executor_id: "worker1".into(),
            request_fingerprint: "a".repeat(64),
        },
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "builder1".into(),
            uid: "sts1".into(),
            resource_version: Some("1".into()),
        }),
        pod: None,
        restart_image: Some("runner:captured".into()),
        restart_runtime_workspace: Some("/home/user/builder1".into()),
        builder_startup_mode: Default::default(),
    }
}

fn archive(source: &BuilderControlTarget) -> BuilderRestartTemplate {
    BuilderRestartTemplate {
        source: source.clone(),
        archive: AppResourceIdentity {
            kind: AppResourceKind::Secret,
            name: "private-restart1".into(),
            uid: "archive1".into(),
            resource_version: Some("4".into()),
        },
        volumes: vec![AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: "workspace".into(),
            uid: "pvc1".into(),
            resource_version: Some("2".into()),
        }],
    }
}

#[tokio::test]
async fn start_keeps_original_archive_when_controller_version_changes() {
    let runtime = Runtime::default();
    let original = target();
    let saved = archive(&original);
    let mut stopped = original.clone();
    stopped
        .workload
        .as_mut()
        .expect("workload")
        .resource_version = Some("7".into());
    let mut checkpoint = serde_json::to_value(&stopped).expect("target");
    checkpoint["builder_restart_template"] = serde_json::to_value(&saved).expect("archive");

    start_from_checkpoint(&runtime, &stopped, &checkpoint)
        .await
        .expect("start");

    assert!(runtime.plain.lock().expect("plain").is_empty());
    assert_eq!(
        *runtime.archived.lock().expect("archive"),
        vec![(stopped, saved)]
    );
}

#[tokio::test]
async fn legacy_checkpoint_does_not_require_a_new_archive() {
    let runtime = Runtime::default();
    let target = target();
    let checkpoint = serde_json::to_value(&target).expect("target");
    start_from_checkpoint(&runtime, &target, &checkpoint)
        .await
        .expect("legacy start");
    assert_eq!(*runtime.plain.lock().expect("plain"), vec![target]);
    assert!(runtime.archived.lock().expect("archive").is_empty());
}

#[tokio::test]
async fn corrupt_or_foreign_archive_is_rejected_before_runtime_write() {
    let runtime = Runtime::default();
    let target = target();
    let mut foreign = archive(&target);
    foreign.source.context.operation_id = "another-operation".into();
    for saved in [
        serde_json::Value::Null,
        serde_json::to_value(foreign).expect("foreign"),
    ] {
        let mut checkpoint = serde_json::to_value(&target).expect("target");
        checkpoint["builder_restart_template"] = saved;
        assert!(
            start_from_checkpoint(&runtime, &target, &checkpoint)
                .await
                .is_err()
        );
    }
    assert!(runtime.plain.lock().expect("plain").is_empty());
    assert!(runtime.archived.lock().expect("archive").is_empty());
}

#[tokio::test]
async fn corrupt_historical_archive_does_not_block_a_confirmed_retained_stopped_builder() {
    let runtime = Runtime {
        stopped: true,
        ..Runtime::default()
    };
    let target = target();
    let mut checkpoint = serde_json::to_value(&target).expect("target");
    checkpoint["builder_restart_template"] = serde_json::Value::Null;
    start_from_checkpoint(&runtime, &target, &checkpoint)
        .await
        .expect("confirmed stop permits actual template reuse");
    assert_eq!(*runtime.plain.lock().expect("plain"), vec![target]);
    assert!(runtime.archived.lock().expect("archive").is_empty());
}

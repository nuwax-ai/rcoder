//! Read-only probe against explicitly prepared Docker/K8s test fixtures.
//! The same app ID must have a dev builder and a prod runtime, each running
//! `app-cli serve`. No fixtures are created, stopped or removed by this test.
//! Requires `--features deploy-host` (and `kubernetes` for the K8s fixture).

#![cfg(feature = "deploy-host")]

use container_runtime_api::{UserAppDeploymentRuntime, UserAppRuntimeReadiness};
use shared_types::{UserAppBusinessReadiness, UserAppNoComputeState, UserappStage};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires explicit RCODER_READINESS_TEST_APP and local runtime fixtures"]
async fn real_readiness_keeps_dev_prod_instances_separate() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("test TLS provider");
    let app = std::env::var("RCODER_READINESS_TEST_APP").expect("explicit test app");
    assert!(app.starts_with("e2e"), "use an isolated e2e fixture");
    let mode = std::env::var("RCODER_READINESS_TEST_RUNTIME").expect("docker or kubernetes");
    let expected =
        std::env::var("RCODER_READINESS_TEST_STATE").expect("running, stopped or missing");
    let runtime: Box<dyn UserAppDeploymentRuntime> = match mode.as_str() {
        "docker" => {
            let config = docker_manager::DockerManagerConfig {
                network_base_name: "bridge".into(),
                ..Default::default()
            };
            let manager = docker_manager::DockerManager::new(config).await.unwrap();
            Box::new(docker_manager::runtime::DockerRuntime::new(Arc::new(
                manager,
            )))
        }
        #[cfg(feature = "kubernetes")]
        "kubernetes" => {
            let namespace = std::env::var("RCODER_K8S_NAMESPACE").expect("explicit test namespace");
            assert!(namespace.starts_with("rcoder-readiness-"));
            Box::new(
                docker_manager::runtime::KubernetesRuntime::new(Default::default())
                    .await
                    .unwrap(),
            )
        }
        other => panic!("unsupported test runtime: {other}"),
    };
    let mut owner_ids = Vec::new();
    for stage in [UserappStage::Dev, UserappStage::Prod] {
        let observed = tokio::time::timeout(
            Duration::from_secs(8),
            runtime.observe_userapp_readiness(&app, stage),
        )
        .await
        .unwrap()
        .unwrap();
        match expected.as_str() {
            "running" => {
                let UserAppRuntimeReadiness::Running(target) = observed else {
                    panic!("expected {stage:?} running, got {observed:?}");
                };
                assert_eq!(target.stage, stage);
                let output = tokio::time::timeout(
                    Duration::from_secs(8),
                    runtime.exec_userapp_readiness(&target),
                )
                .await
                .unwrap()
                .unwrap()
                .expect("captured instance remains present");
                assert_eq!(output.exit_code, 0, "{}", output.stderr);
                let snapshot: UserAppBusinessReadiness =
                    serde_json::from_str(&output.stdout).unwrap();
                let owner = snapshot
                    .runtime_instance_id
                    .expect("app-cli owner identity");
                println!(
                    "{stage:?}: physical={}, owner={owner}, status={:?}",
                    target.instance.physical_id(),
                    snapshot.status
                );
                owner_ids.push(owner);
            }
            "stopped" => assert_eq!(
                observed,
                UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Stopped)
            ),
            "missing" => assert_eq!(
                observed,
                UserAppRuntimeReadiness::NotRunning(UserAppNoComputeState::Missing)
            ),
            other => panic!("unknown expected state: {other}"),
        }
    }
    if expected == "running" {
        assert_ne!(owner_ids[0], owner_ids[1], "dev exec must never query prod");
    }
}

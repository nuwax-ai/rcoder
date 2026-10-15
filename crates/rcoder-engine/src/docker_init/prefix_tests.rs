//! Prefix selection is configuration-only; these tests never contact a runtime.
use super::get_container_prefixes;
use crate::config::{AppConfig, DockerConfig};
use shared_types::{KubernetesConfig, MultiImageConfig, ServiceType};

fn kubernetes_prefixes() -> KubernetesConfig {
    serde_json::from_value(serde_json::json!({
        "services": {
            "web-agent-runner": {
                "service_type": "WebAgentRunner",
                "image": "registry.invalid/web:test",
                "image_tag_prefix": "explicit-k8s-web",
                "resource_limits": {"memory": 536870912.0, "cpu": 1.0}
            },
            "computer-agent-runner": {
                "service_type": "ComputerAgentRunner",
                "image": "registry.invalid/computer:test",
                "image_tag_prefix": "explicit-k8s-computer",
                "resource_limits": {"memory": 536870912.0, "cpu": 1.0}
            }
        }
    }))
    .unwrap()
}

fn docker_prefixes() -> DockerConfig {
    let mut multi = shared_types::create_default_multi_image_config();
    multi.services.clear();
    let mut web = shared_types::default_rcoder_service_config();
    web.image_tag_prefix = Some("explicit-docker-web".to_owned());
    multi
        .services
        .insert(ServiceType::WebAgentRunner.to_string(), web);
    let mut computer = shared_types::default_agent_runner_service_config();
    computer.image_tag_prefix = Some("explicit-docker-computer".to_owned());
    multi
        .services
        .insert(ServiceType::ComputerAgentRunner.to_string(), computer);
    DockerConfig {
        multi_image_config: Some(multi),
        ..DockerConfig::default()
    }
}

fn docker_multi(config: &mut DockerConfig) -> &mut MultiImageConfig {
    config.multi_image_config.as_mut().unwrap()
}

#[tokio::test]
async fn complete_kubernetes_prefixes_do_not_require_docker_configuration() {
    let config = AppConfig {
        docker_config: None,
        kubernetes_config: Some(kubernetes_prefixes()),
        ..AppConfig::default()
    };
    assert_eq!(
        get_container_prefixes(&config).await.unwrap(),
        (
            "explicit-k8s-web".to_owned(),
            "explicit-k8s-computer".to_owned()
        )
    );
}

#[tokio::test]
async fn complete_kubernetes_prefixes_do_not_validate_unused_docker_services() {
    let mut docker = docker_prefixes();
    docker_multi(&mut docker).services.clear();
    let config = AppConfig {
        docker_config: Some(docker),
        kubernetes_config: Some(kubernetes_prefixes()),
        ..AppConfig::default()
    };
    assert_eq!(
        get_container_prefixes(&config).await.unwrap(),
        (
            "explicit-k8s-web".to_owned(),
            "explicit-k8s-computer".to_owned()
        )
    );
}

#[tokio::test]
async fn explicit_kubernetes_prefixes_precede_valid_docker_prefixes() {
    let config = AppConfig {
        docker_config: Some(docker_prefixes()),
        kubernetes_config: Some(kubernetes_prefixes()),
        ..AppConfig::default()
    };
    assert_eq!(
        get_container_prefixes(&config).await.unwrap(),
        (
            "explicit-k8s-web".to_owned(),
            "explicit-k8s-computer".to_owned()
        )
    );
}

#[tokio::test]
async fn partial_kubernetes_prefixes_resolve_only_the_missing_docker_service() {
    for k8s_kind in [
        ServiceType::WebAgentRunner,
        ServiceType::ComputerAgentRunner,
    ] {
        let mut k8s = kubernetes_prefixes();
        k8s.services
            .retain(|_, config| config.service_type == k8s_kind);
        let mut docker = docker_prefixes();
        // The already-resolved K8s key must not demand an unrelated Docker key.
        docker_multi(&mut docker)
            .services
            .remove(&k8s_kind.to_string());
        let config = AppConfig {
            docker_config: Some(docker),
            kubernetes_config: Some(k8s),
            ..AppConfig::default()
        };
        let expected = if k8s_kind == ServiceType::WebAgentRunner {
            (
                "explicit-k8s-web".to_owned(),
                "explicit-docker-computer".to_owned(),
            )
        } else {
            (
                "explicit-docker-web".to_owned(),
                "explicit-k8s-computer".to_owned(),
            )
        };
        assert_eq!(get_container_prefixes(&config).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn missing_kubernetes_services_preserve_existing_docker_lookup() {
    for k8s in [None, Some(KubernetesConfig::default())] {
        let mut docker = docker_prefixes();
        let web = docker_multi(&mut docker)
            .services
            .remove(&ServiceType::WebAgentRunner.to_string())
            .unwrap();
        // The existing web-agent-runner legacy key remains a selector contract.
        docker_multi(&mut docker)
            .services
            .insert("rcoder".into(), web);
        let config = AppConfig {
            docker_config: Some(docker),
            kubernetes_config: k8s,
            ..AppConfig::default()
        };
        assert_eq!(
            get_container_prefixes(&config).await.unwrap(),
            (
                "explicit-docker-web".to_owned(),
                "explicit-docker-computer".to_owned()
            )
        );
    }
}

#[tokio::test]
async fn missing_prefix_without_docker_keeps_explicit_configuration_error() {
    for k8s in [None, Some(KubernetesConfig::default())] {
        let config = AppConfig {
            docker_config: None,
            kubernetes_config: k8s,
            ..AppConfig::default()
        };
        let error = get_container_prefixes(&config).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Docker config is required for container prefix")
        );
    }
}

#[tokio::test]
async fn required_disabled_docker_fallback_still_fails_without_inventing_a_prefix() {
    let mut k8s = kubernetes_prefixes();
    k8s.services
        .remove(&ServiceType::ComputerAgentRunner.to_string());
    let mut docker = docker_prefixes();
    docker_multi(&mut docker)
        .services
        .get_mut(&ServiceType::ComputerAgentRunner.to_string())
        .unwrap()
        .enabled = false;
    let config = AppConfig {
        docker_config: Some(docker),
        kubernetes_config: Some(k8s),
        ..AppConfig::default()
    };
    let error = get_container_prefixes(&config).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Failed to get ComputerAgentRunner service config")
    );
    assert!(error.to_string().contains("not enabled"));
}

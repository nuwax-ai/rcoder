use super::*;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn sts(image: &str) -> StatefulSet {
    StatefulSet {
        metadata: ObjectMeta {
            name: Some("agent-x".into()),
            uid: Some("uid-1".into()),
            resource_version: Some("7".into()),
            labels: Some([("rcoder.io/app".to_string(), "x".to_string())].into()),
            annotations: Some([(TEMPLATE_HASH_ANNOTATION.to_string(), "h1".to_string())].into()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            template: PodTemplateSpec {
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: "agent".into(),
                        image: Some(image.into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn identical_winner_is_adoptable_even_with_stale_hash_annotation() {
    let mut winner = sts("img:1");
    winner.metadata.annotations =
        Some([(TEMPLATE_HASH_ANNOTATION.to_string(), "legacy-h".to_string())].into());
    validate_agent_statefulset(&winner, &sts("img:1")).expect("hash 注解只作记账不参与比对");
}

#[test]
fn launch_content_drift_rejects_winner() {
    assert!(validate_agent_statefulset(&sts("img:OLD"), &sts("img:1")).is_err());
}

#[test]
fn identity_label_drift_rejects_winner() {
    let mut winner = sts("img:1");
    winner.metadata.labels = Some([("rcoder.io/app".to_string(), "OTHER".to_string())].into());
    assert!(validate_agent_statefulset(&winner, &sts("img:1")).is_err());
}

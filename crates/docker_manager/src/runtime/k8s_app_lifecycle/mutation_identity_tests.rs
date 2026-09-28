use super::*;

/// 镜像滚动版 restart patch：同一写携带镜像+重启注解+replicas；
/// containers 只含 name/image 两个键（Strategic merge-by-name 语义，
/// 误用 Merge patch 会整组替换 containers——此处固化形状防回退）。
#[test]
fn restart_patch_carries_image_annotation_and_replicas_in_one_write() {
    let patch = app_restart_patch("op-restart", Some("registry.test/app-runtime:0.2.0"));
    assert_eq!(
        patch["metadata"]["annotations"][WAKE_ON_TRAFFIC_ANNOTATION],
        "true"
    );
    assert_eq!(patch["spec"]["replicas"], 1);
    assert_eq!(
        patch["spec"]["template"]["metadata"]["annotations"]["rcoder.io/restart-operation"],
        "op-restart"
    );
    let containers = patch["spec"]["template"]["spec"]["containers"]
        .as_array()
        .expect("containers array");
    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0]["name"], APP_CONTAINER_NAME);
    assert_eq!(containers[0]["image"], "registry.test/app-runtime:0.2.0");
    assert_eq!(
        containers[0].as_object().expect("container object").len(),
        2,
        "container entry must carry only name+image"
    );
}

/// 无镜像版保持既有形状：template 只含 metadata 注解，不出现 containers
/// 键（走 Merge patch 的判定依据）。
#[test]
fn restart_patch_without_image_keeps_plain_shape() {
    let patch = app_restart_patch("op-restart", None);
    assert_eq!(patch["spec"]["replicas"], 1);
    assert_eq!(
        patch["spec"]["template"]["metadata"]["annotations"]["rcoder.io/restart-operation"],
        "op-restart"
    );
    assert!(
        patch["spec"]["template"].get("spec").is_none(),
        "no container template spec without an image roll"
    );
}

#[test]
fn compute_start_patch_carries_image_only_when_present() {
    let plain = app_compute_start_patch("{\"op\":\"a\"}", None);
    assert_eq!(plain["spec"]["replicas"], 1);
    assert_eq!(
        plain["metadata"]["annotations"]["rcoder.io/compute-start-receipt"],
        "{\"op\":\"a\"}"
    );
    assert!(plain["spec"].get("template").is_none());

    let rolled = app_compute_start_patch("{\"op\":\"b\"}", Some("registry.test/app-runtime:9"));
    let containers = rolled["spec"]["template"]["spec"]["containers"]
        .as_array()
        .expect("containers array");
    assert_eq!(containers[0]["name"], APP_CONTAINER_NAME);
    assert_eq!(containers[0]["image"], "registry.test/app-runtime:9");
    assert_eq!(
        rolled["metadata"]["annotations"][WAKE_ON_TRAFFIC_ANNOTATION],
        "true"
    );
}

#[test]
fn conditional_workload_patch_retains_requested_changes_and_captured_identity() {
    let metadata = k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
        name: Some("rcoder-app-one".into()),
        uid: Some("original-uid".into()),
        resource_version: Some("17".into()),
        ..Default::default()
    };
    let identity = app_mutation_identity("rcoder-app-one".into(), &metadata).expect("identity");
    for change in [
        serde_json::json!({"spec":{"replicas":0}}),
        serde_json::json!({"metadata":{"annotations":{"rcoder.io/wake-on-traffic":"false"}}}),
        serde_json::json!({"spec":{"template":{"metadata":{"annotations":{"restart":"now"}}}}}),
    ] {
        let patch = condition_app_patch(&identity, change.clone()).expect("patch");
        assert_eq!(patch["metadata"]["uid"], "original-uid");
        assert_eq!(patch["metadata"]["resourceVersion"], "17");
        if let Some(spec) = change.get("spec") {
            assert_eq!(&patch["spec"], spec);
        }
        if let Some(annotations) = change.pointer("/metadata/annotations") {
            assert_eq!(&patch["metadata"]["annotations"], annotations);
        }
    }
    let mut missing = metadata.clone();
    missing.uid = None;
    assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
    let mut missing = metadata;
    missing.resource_version = None;
    assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
    assert!(app_mutation_identity("replacement-name".into(), &missing).is_err());
}

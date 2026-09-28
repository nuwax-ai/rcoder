use super::*;

#[test]
fn endpoint_requires_captured_id_lifecycle_family_and_running_address() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "clear".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let resource = AppResourceIdentity {
        kind: AppResourceKind::Container,
        name: "builder-app".into(),
        uid: "physical-original".into(),
        resource_version: None,
    };
    let mut labels = context.resource_metadata();
    labels.insert(
        "service-type".into(),
        ServiceType::UserappBuilder.to_string(),
    );
    labels.insert("identifier".into(), "app".into());
    let original = serde_json::json!({
        "Id": resource.uid, "Config": {"Labels": labels}, "State": {"Running": true},
        "HostConfig": {"NetworkMode": "main"},
        "NetworkSettings": {"Networks": {"main": {"IPAddress": "172.20.0.4"}}}
    });
    let inspect = serde_json::from_value(original.clone()).expect("inspect fixture");
    let endpoint =
        workspace_endpoint_from_container(&inspect, &resource, &context).expect("physical target");
    assert_eq!(endpoint.container_id, "physical-original");
    assert_eq!(
        endpoint.base_url(),
        format!("http://172.20.0.4:{}", shared_types::AGENT_FILE_SERVER_PORT)
    );
    for (pointer, value) in [
        ("/Id", serde_json::json!("replacement")),
        (
            "/Config/Labels/rcoder.io~1lifecycle-id",
            serde_json::json!("next-life"),
        ),
        (
            "/Config/Labels/service-type",
            serde_json::json!(ServiceType::Userapp.to_string()),
        ),
        (
            "/Config/Labels/identifier",
            serde_json::json!("another-app"),
        ),
        ("/State/Running", serde_json::json!(false)),
        (
            "/NetworkSettings/Networks/main/IPAddress",
            serde_json::json!(""),
        ),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).expect("fixture field") = value;
        let inspect = serde_json::from_value(changed).expect("changed fixture");
        assert!(
            workspace_endpoint_from_container(&inspect, &resource, &context).is_err(),
            "accepted changed field {pointer}"
        );
    }
}

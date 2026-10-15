use super::*;

#[test]
fn http_port_label_overrides_docker_tcp_port_observation() {
    let mut ports = vec![AppPortStatus {
        name: String::new(),
        port: 9080,
        expose_type: ExposeType::Tcp,
        external_port: Some(32080),
    }];
    merge_http_port_labels(&mut ports, "9080:http,60000:http,5432:tcp,9999:unknown");
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].expose_type, ExposeType::Http);
    assert_eq!(ports[0].external_port, Some(32080));
    assert_eq!(ports[1].port, 60000);
    assert_eq!(ports[1].expose_type, ExposeType::Http);
}

#[test]
fn configuration_exec_requires_physical_lifecycle_and_generation_match() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app1".into(),
        lifecycle_id: "life1".into(),
        operation_id: "operation1".into(),
        executor_id: "executor1".into(),
        request_fingerprint: "a".repeat(64),
    };
    let target = shared_types::RuntimeConfigurationTarget {
        physical_uid: "container1".into(),
        deployment_generation: "generation1".into(),
    };
    let mut labels = context.resource_metadata();
    labels.insert("managed-by".into(), "rcoder-app-manager".into());
    labels.insert(
        "service-type".into(),
        shared_types::ServiceType::Userapp.to_string(),
    );
    labels.insert(
        shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
        "app1".into(),
    );
    let fixture = serde_json::json!({
        "Id":"container1", "Config":{"Labels": labels,
        "Env":[format!("{}=generation1",shared_types::APP_DEPLOY_GENERATION_ID)]}
    });
    let inspected = serde_json::from_value(fixture.clone()).unwrap();
    validate_configuration_exec_target(&context, &target, &inspected).unwrap();
    for (pointer, replacement) in [
        ("/Id", serde_json::json!("container2")),
        (
            "/Config/Labels/rcoder.io~1lifecycle-id",
            serde_json::json!("life2"),
        ),
        (
            "/Config/Labels/service-type",
            serde_json::json!(shared_types::ServiceType::UserappBuilder.to_string()),
        ),
        ("/Config/Env", serde_json::json!([])),
        (
            "/Config/Env",
            serde_json::json!([format!(
                "{}=generation2",
                shared_types::APP_DEPLOY_GENERATION_ID
            )]),
        ),
        (
            "/Config/Env",
            serde_json::json!([
                format!("{}=generation1", shared_types::APP_DEPLOY_GENERATION_ID),
                format!("{}=generation2", shared_types::APP_DEPLOY_GENERATION_ID)
            ]),
        ),
    ] {
        let mut changed = fixture.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let inspected = serde_json::from_value(changed).unwrap();
        assert!(
            validate_configuration_exec_target(&context, &target, &inspected).is_err(),
            "{pointer}"
        );
    }
}

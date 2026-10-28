use super::*;

#[tokio::test]
async fn builder_inventory_keeps_exact_app_label_and_never_splits_dash_identifiers() {
    let fixture = fixture(Case::ServiceOnly).await;
    let id = "old-app-with-dash";
    let name = fixture
        .runtime
        .agent_workload_name(id, &ServiceType::UserappBuilder)
        .unwrap();
    {
        let mut objects = fixture.objects.lock().unwrap();
        let service = &mut objects[0];
        service["metadata"]["name"] = format!("{name}-svc").into();
        service["metadata"]["labels"] =
            serde_json::to_value(super::super::super::k8s_service::build_standard_labels(
                id,
                &ServiceType::UserappBuilder,
            ))
            .unwrap();
        service["metadata"]["labels"]["rcoder.io/app-id"] = "app-with-dash".into();
    }
    let snapshot = fixture
        .runtime
        .capture_agent_stop(id, &ServiceType::UserappBuilder)
        .await
        .unwrap();
    assert_eq!(snapshot.identifier, id);
    assert_eq!(snapshot.app_id_hint.as_deref(), Some("app-with-dash"));
    assert_ne!(snapshot.app_id_hint.as_deref(), Some("dash"));
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH "))
    );
}

#[tokio::test]
async fn conflicting_or_missing_builder_hints_preserve_other_inventory_scopes() {
    let fixture = fixture(Case::ServiceOnly).await;
    let id = "legacy-user-app";
    let name = fixture
        .runtime
        .agent_workload_name(id, &ServiceType::UserappBuilder)
        .unwrap();
    {
        let mut objects = fixture.objects.lock().unwrap();
        let mut first = objects[0].clone();
        first["metadata"]["name"] = format!("{name}-svc").into();
        first["metadata"]["labels"] =
            serde_json::to_value(super::super::super::k8s_service::build_standard_labels(
                id,
                &ServiceType::UserappBuilder,
            ))
            .unwrap();
        first["metadata"]["labels"]["rcoder.io/app-id"] = "actual-app".into();
        let mut second = first.clone();
        second["metadata"]["name"] = format!("{name}-headless").into();
        second["metadata"]["uid"] = "other-service".into();
        second["metadata"]["labels"]["rcoder.io/app-id"] = "foreign-app".into();
        let mut unrelated = objects[0].clone();
        unrelated["metadata"]["labels"]["rcoder.io/app-id"] = "unrelated-original-hint".into();
        *objects = vec![first, second, unrelated];
    }
    let inventory = fixture.runtime.agent_resource_inventory().await.unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(
        inventory
            .iter()
            .find(|item| item.identifier == id)
            .unwrap()
            .app_id_hint
            .is_none()
    );
    assert_eq!(
        inventory
            .iter()
            .find(|item| item.identifier == "owner")
            .unwrap()
            .app_id_hint
            .as_deref(),
        Some("unrelated-original-hint")
    );
    {
        let mut objects = fixture.objects.lock().unwrap();
        objects[1]["metadata"]["labels"]
            .as_object_mut()
            .unwrap()
            .remove("rcoder.io/app-id");
    }
    let missing = fixture
        .runtime
        .capture_agent_stop(id, &ServiceType::UserappBuilder)
        .await
        .unwrap();
    assert!(
        missing.app_id_hint.is_none(),
        "one missing hint cannot be repaired by another resource's label"
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH "))
    );
}

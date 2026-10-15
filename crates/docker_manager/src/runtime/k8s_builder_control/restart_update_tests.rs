//! Frozen restart configuration, exact API write, and uncertain-write retry.
use super::*;
use container_runtime_api::AgentContainerRuntime as _;

async fn captured(
    runtime: &KubernetesRuntime,
    params: &container_runtime_api::ContainerCreateParams,
) -> BuilderControlTarget {
    runtime
        .capture_builder_compute_with_binding(
            params.execution_context.as_ref().expect("context"),
            params.resource_binding.as_ref(),
            false,
        )
        .await
        .expect("captured builder")
}

#[tokio::test]
async fn explicit_restart_applies_frozen_template_once_and_retains_token_pvc_and_user_resources() {
    for lose_response in [false, true] {
        let (mut runtime, params, state, server) = resume_tests::fixture(None).await;
        let config: shared_types::K8sServiceConfig = serde_json::from_value(serde_json::json!({
            "service_type":ServiceType::UserappBuilder,"enabled":true,
            "environment":{"PLATFORM_SETTING":"frozen-value"},
            "command":["/updated-platform-entrypoint"]
        }))
        .expect("platform config");
        runtime
            .config
            .kubernetes_config
            .services
            .insert(ServiceType::UserappBuilder.to_string(), config);
        state.lock().expect("state").allow_writes = true;
        let mut source = captured(&runtime, &params).await;
        source.restart_image = Some("paired-runtime:new".into());
        source.restart_runtime_workspace = Some(shared_types::paths::userapp_dev_workspace("app"));
        let template = runtime
            .archive_builder_restart(&source)
            .await
            .expect("archive")
            .expect("K8s archive");
        let original_spec = state.lock().expect("state").sts["spec"]["template"]["spec"].clone();
        // The already frozen restart cannot silently pick a later platform edit.
        runtime.config.kubernetes_config.services.clear();
        let repeated = runtime
            .archive_builder_restart(&source)
            .await
            .expect("same operation archive retry")
            .expect("archive");
        assert_eq!(
            repeated, template,
            "the first stored intent must win over a rebuilt recipe"
        );
        assert!(
            runtime
                .apply_builder_control(&source, false)
                .await
                .expect("actual stop")
                .is_none()
        );
        {
            let mut state = state.lock().expect("state");
            assert_eq!(state.sts["spec"]["replicas"], 0);
            assert!(state.pod.is_null());
            state.lose_next_put_response = lose_response;
        }
        let mut stopped = captured(&runtime, &params).await;
        stopped.restart_image = source.restart_image.clone();
        stopped.restart_runtime_workspace = source.restart_runtime_workspace.clone();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.start_builder_control_with_restart(&stopped, &template),
        )
        .await
        .expect("start budget");
        if lose_response {
            assert!(first.is_err(), "a lost write response must remain unknown");
        } else {
            assert_eq!(
                first.expect("started").expect("ready").container_id,
                "pod-updated"
            );
        }
        let mut resumed = captured(&runtime, &params).await;
        resumed.restart_image = source.restart_image.clone();
        resumed.restart_runtime_workspace = source.restart_runtime_workspace.clone();
        let retry = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.start_builder_control_with_restart(&resumed, &template),
        )
        .await
        .expect("retry budget");
        assert_eq!(
            retry
                .expect("same request observed")
                .expect("ready")
                .container_id,
            "pod-updated"
        );
        let state = state.lock().expect("state");
        assert_eq!(
            state.puts.len(),
            1,
            "same intent must not replay its acknowledged PUT"
        );
        assert_eq!(
            state.patches.len(),
            1,
            "one acknowledged Stop must precede the sole replacement PUT"
        );
        assert_eq!(state.patches[0]["spec"]["replicas"], 0);
        let replacement = &state.puts[0];
        assert_eq!(replacement["metadata"]["uid"], "sts-original");
        let candidate = &replacement["spec"]["template"]["spec"];
        assert_eq!(candidate["containers"][0]["image"], "paired-runtime:new");
        assert_eq!(
            candidate["containers"][0]["command"],
            serde_json::json!(["/updated-platform-entrypoint"])
        );
        assert_eq!(
            candidate["containers"][0]["resources"],
            original_spec["containers"][0]["resources"]
        );
        assert_eq!(
            candidate["volumes"][0]["persistentVolumeClaim"]["claimName"],
            original_spec["volumes"][0]["persistentVolumeClaim"]["claimName"]
        );
        assert_eq!(state.pvc["metadata"]["uid"], "pvc-original");
        let env = candidate["containers"][0]["env"]
            .as_array()
            .expect("candidate env");
        assert!(
            env.iter().any(
                |entry| entry["name"] == "PLATFORM_SETTING" && entry["value"] == "frozen-value"
            )
        );
        assert!(
            env.iter()
                .any(|entry| entry["name"] == "APP_CLI_DEPLOY_TOKEN"
                    && entry["value"] == "original-credential-never-replaced")
        );
        let controller_name = state.sts["metadata"]["name"]
            .as_str()
            .expect("controller name");
        for request in &state.requests {
            let mut parts = request.split_whitespace();
            let method = parts.next().expect("method");
            let path = parts
                .next()
                .expect("path")
                .split('?')
                .next()
                .expect("resource path");
            let expected = method == "GET"
                || method == "POST" && path == "/api/v1/namespaces/resume-contract/secrets"
                || matches!(method, "PUT" | "PATCH")
                    && path
                        == format!(
                            "/apis/apps/v1/namespaces/resume-contract/statefulsets/{controller_name}"
                        );
            assert!(expected, "unexpected resource mutation: {request}");
        }
        server.abort();
    }
}

#[tokio::test]
async fn restart_refuses_live_old_pod_and_changed_pvc_before_configuration_write() {
    for changed_pvc in [false, true] {
        let (runtime, params, state, server) = resume_tests::fixture(None).await;
        state.lock().expect("state").allow_writes = true;
        let mut source = captured(&runtime, &params).await;
        source.restart_image = Some("paired-runtime:new".into());
        let template = runtime
            .archive_builder_restart(&source)
            .await
            .expect("archive")
            .expect("K8s archive");
        {
            let mut state = state.lock().expect("state");
            state.sts["spec"]["replicas"] = 0.into();
            state.sts["metadata"]["resourceVersion"] = "9".into();
            if changed_pvc {
                state.pod = serde_json::Value::Null;
                state.pvc["metadata"]["uid"] = "replacement-pvc".into();
            }
        }
        let mut target = captured(&runtime, &params).await;
        target.restart_image = source.restart_image.clone();
        let outcome = runtime
            .start_builder_control_with_restart(&target, &template)
            .await;
        assert!(outcome.is_err(), "unsafe restart must be rejected");
        assert!(state.lock().expect("state").puts.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn non_rollout_restart_freezes_existing_configuration() {
    let (mut runtime, params, state, server) = resume_tests::fixture(Some("release")).await;
    state.lock().expect("state").allow_writes = true;
    let source = captured(&runtime, &params).await;
    let template = runtime
        .archive_builder_restart(&source)
        .await
        .expect("archive")
        .expect("K8s archive");
    runtime.config.service_account_name = "later-platform-change".into();
    {
        let mut state = state.lock().expect("state");
        state.sts["spec"]["replicas"] = 0.into();
        state.sts["metadata"]["resourceVersion"] = "9".into();
        state.pod = serde_json::Value::Null;
    }
    let target = captured(&runtime, &params).await;
    let outcome = runtime
        .start_builder_control_with_restart(&target, &template)
        .await;
    assert!(
        outcome.is_ok(),
        "repair must use the frozen original config: {outcome:?}"
    );
    let state = state.lock().expect("state");
    let candidate = &state.puts[0]["spec"]["template"]["spec"];
    assert_eq!(candidate["containers"][0]["image"], "old-release:1");
    assert_eq!(candidate["serviceAccountName"], "test");
    assert!(
        candidate["containers"][0]["env"]
            .as_array()
            .expect("env")
            .iter()
            .any(|entry| entry["name"] == "APPLICATION_OWNED_SETTING" && entry["value"] == "keep")
    );
    server.abort();
}

#[tokio::test]
async fn missing_or_damaged_archive_does_not_block_confirmed_retained_stop() {
    for damaged in [false, true] {
        let (runtime, params, state, server) = resume_tests::fixture(None).await;
        state.lock().expect("state").allow_writes = true;
        let mut source = captured(&runtime, &params).await;
        source.restart_image = Some("paired-runtime:new".into());
        source.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
        let template = runtime
            .archive_builder_restart(&source)
            .await
            .expect("archive")
            .expect("template");
        assert!(
            runtime
                .apply_builder_control(&source, false)
                .await
                .expect("confirmed stop")
                .is_none()
        );
        {
            let mut state = state.lock().expect("state");
            if damaged {
                // A valid base64 payload with damaged historical JSON.
                use base64::Engine as _;
                state
                    .archives
                    .get_mut(&template.archive.name)
                    .expect("archive")["data"]["template"] =
                    base64::engine::general_purpose::STANDARD
                        .encode(b"{broken history")
                        .into();
            } else {
                state.archives.clear();
            }
        }
        let mut target = captured(&runtime, &params).await;
        target.restart_image = source.restart_image.clone();
        target.builder_startup_mode = source.builder_startup_mode;
        let result = runtime
            .start_builder_control_with_options(
                &target,
                &shared_types::BuilderStartOptions {
                    startup_mode: source.builder_startup_mode,
                    restart_template: Some(template),
                },
            )
            .await;
        assert!(
            result.is_ok(),
            "stopped retained data can continue without old metadata: {result:?}"
        );
        let state = state.lock().expect("state");
        assert!(
            state.puts.is_empty(),
            "lost recipe must not invent a new full configuration"
        );
        assert_eq!(
            state.patches.len(),
            2,
            "one confirmed Stop and one captured management wake"
        );
        assert_eq!(state.pvc["metadata"]["uid"], "pvc-original");
        server.abort();
    }
}

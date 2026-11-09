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
        // Legacy pre-upgrade shape (like a builder still carrying request=64Mi
        // under a 6Gi limit): the non-memory fields are recognisable values so
        // any wholesale resource replacement would be caught.
        {
            let mut state = state.lock().expect("state");
            state.sts["spec"]["template"]["spec"]["containers"][0]["resources"] = serde_json::json!({
                "limits": {"cpu": "1500m", "ephemeral-storage": "3Gi", "memory": "6Gi"},
                "requests": {"cpu": "300m", "ephemeral-storage": "700Mi", "memory": "64Mi"}
            });
        }
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
        // A rollout candidate refreshes exactly one resource field: the
        // platform-managed memory request, recomputed as min(1Gi, limit) from
        // the retained 6Gi limit. Every other resource field — all limits and
        // the cpu/ephemeral-storage requests — is retained verbatim.
        let mut expected_resources = original_spec["containers"][0]["resources"].clone();
        expected_resources["requests"]["memory"] = "1Gi".into();
        assert_eq!(
            candidate["containers"][0]["resources"], expected_resources,
            "only requests.memory may follow the current platform policy; all other resource fields are retained"
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
    // Legacy resource shape: a recovery that does not adopt the current
    // platform template must not refresh the platform resource policy either.
    let legacy_resources = serde_json::json!({
        "limits": {"cpu": "1500m", "memory": "6Gi"},
        "requests": {"cpu": "300m", "memory": "64Mi"}
    });
    state.lock().expect("state").sts["spec"]["template"]["spec"]["containers"][0]["resources"] =
        legacy_resources.clone();
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
    assert_eq!(
        candidate["containers"][0]["resources"], legacy_resources,
        "recovery without a rollout keeps the original resources verbatim"
    );
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

/// The platform memory request is recomputed through the real candidate
/// chain, so a small retained limit keeps request == limit (never 1Gi above
/// it, which K8s server-side validation would reject).
#[tokio::test]
async fn explicit_restart_small_memory_limit_keeps_refreshed_request_within_limit() {
    let (runtime, params, state, server) = resume_tests::fixture(None).await;
    state.lock().expect("state").allow_writes = true;
    {
        let mut state = state.lock().expect("state");
        state.sts["spec"]["template"]["spec"]["containers"][0]["resources"] = serde_json::json!({
            "limits": {"cpu": "1500m", "memory": "512Mi"},
            "requests": {"cpu": "300m", "memory": "64Mi"}
        });
    }
    let mut source = captured(&runtime, &params).await;
    source.restart_image = Some("paired-runtime:new".into());
    let template = runtime
        .archive_builder_restart(&source)
        .await
        .expect("archive")
        .expect("K8s archive");
    assert!(
        runtime
            .apply_builder_control(&source, false)
            .await
            .expect("actual stop")
            .is_none()
    );
    {
        let state = state.lock().expect("state");
        assert_eq!(state.sts["spec"]["replicas"], 0);
        assert!(state.pod.is_null());
    }
    let mut stopped = captured(&runtime, &params).await;
    stopped.restart_image = source.restart_image.clone();
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.start_builder_control_with_restart(&stopped, &template),
    )
    .await
    .expect("start budget");
    assert_eq!(
        started.expect("started").expect("ready").container_id,
        "pod-updated"
    );
    let state = state.lock().expect("state");
    let resources = &state.puts[0]["spec"]["template"]["spec"]["containers"][0]["resources"];
    assert_eq!(
        resources["requests"]["memory"], "512Mi",
        "request must cap at the retained limit"
    );
    assert_eq!(resources["limits"]["memory"], "512Mi");
    assert_eq!(resources["requests"]["cpu"], "300m");
    assert_eq!(resources["limits"]["cpu"], "1500m");
    server.abort();
}

/// Candidate construction matrix, asserted on the frozen archive payload so
/// each shape exercises the real capture→candidate→freeze chain.
#[tokio::test]
async fn restart_candidate_memory_request_follows_platform_policy_matrix() {
    struct Case {
        limits: serde_json::Value,
        requests: serde_json::Value,
        expected_requests: serde_json::Value,
    }
    let cases = [
        // Large limits in every legal Quantity format cap the request at 1Gi
        // while the original limit string is retained verbatim.
        Case {
            limits: serde_json::json!({"cpu": "1", "memory": "8.0Gi"}),
            requests: serde_json::json!({"cpu": "300m", "memory": "64Mi"}),
            expected_requests: serde_json::json!({"cpu": "300m", "memory": "1Gi"}),
        },
        Case {
            limits: serde_json::json!({"memory": "1.5Gi"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1Gi"}),
        },
        Case {
            limits: serde_json::json!({"memory": "8e9"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1Gi"}),
        },
        Case {
            limits: serde_json::json!({"memory": "1536Mi"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1Gi"}),
        },
        // At or below the 1GiB boundary the limit itself is the request.
        Case {
            limits: serde_json::json!({"memory": "1024Mi"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1024Mi"}),
        },
        Case {
            limits: serde_json::json!({"memory": "1Gi"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1Gi"}),
        },
        Case {
            limits: serde_json::json!({"memory": "512Mi"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "512Mi"}),
        },
        // Half-byte values the shared parser rounds up to exactly 1GiB keep
        // the original limit (a 1Gi request would exceed it).
        Case {
            limits: serde_json::json!({"memory": "1073741823.6"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1073741823.6"}),
        },
        Case {
            limits: serde_json::json!({"memory": "1073741823.9"}),
            requests: serde_json::json!({"memory": "64Mi"}),
            expected_requests: serde_json::json!({"memory": "1073741823.9"}),
        },
        // No memory limit: nothing is invented, requests stay as configured.
        Case {
            limits: serde_json::json!({"cpu": "1500m"}),
            requests: serde_json::json!({"cpu": "300m"}),
            expected_requests: serde_json::json!({"cpu": "300m"}),
        },
        // No requests map at all: only the memory entry is created.
        Case {
            limits: serde_json::json!({"memory": "6Gi"}),
            requests: serde_json::Value::Null,
            expected_requests: serde_json::json!({"memory": "1Gi"}),
        },
    ];
    for case in &cases {
        let (runtime, params, state, server) = resume_tests::fixture(None).await;
        state.lock().expect("state").allow_writes = true;
        {
            let mut state = state.lock().expect("state");
            let resources =
                &mut state.sts["spec"]["template"]["spec"]["containers"][0]["resources"];
            *resources = serde_json::json!({"limits": case.limits.clone()});
            if !case.requests.is_null() {
                resources["requests"] = case.requests.clone();
            }
        }
        let mut source = captured(&runtime, &params).await;
        source.restart_image = Some("paired-runtime:new".into());
        let template = runtime
            .archive_builder_restart(&source)
            .await
            .expect("archive")
            .expect("K8s archive");
        let resources = {
            use base64::Engine as _;
            let state = state.lock().expect("state");
            let payload = state.archives[&template.archive.name]["data"]["template"]
                .as_str()
                .expect("archive payload")
                .to_owned();
            let archive: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .expect("base64"),
            )
            .expect("archive json");
            archive["candidate_pod_spec"]["containers"][0]["resources"].clone()
        };
        let mut expected = serde_json::json!({"limits": case.limits.clone()});
        if !case.expected_requests.is_null() {
            expected["requests"] = case.expected_requests.clone();
        }
        assert_eq!(
            resources, expected,
            "limits={}: candidate must refresh only the platform memory request",
            case.limits,
        );
        server.abort();
    }
    // resources absent entirely: no default resource shape is invented.
    let (runtime, params, state, server) = resume_tests::fixture(None).await;
    state.lock().expect("state").allow_writes = true;
    state.lock().expect("state").sts["spec"]["template"]["spec"]["containers"][0]["resources"] =
        serde_json::Value::Null;
    let mut source = captured(&runtime, &params).await;
    source.restart_image = Some("paired-runtime:new".into());
    let template = runtime
        .archive_builder_restart(&source)
        .await
        .expect("archive")
        .expect("K8s archive");
    {
        use base64::Engine as _;
        let state = state.lock().expect("state");
        let payload = state.archives[&template.archive.name]["data"]["template"]
            .as_str()
            .expect("archive payload");
        let archive: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(payload)
                .expect("base64"),
        )
        .expect("archive json");
        assert!(
            archive["candidate_pod_spec"]["containers"][0]["resources"].is_null(),
            "no original resources means no invented defaults"
        );
    }
    server.abort();
}

/// An operation whose archive was frozen by the previous release (candidate
/// still carries the old 64Mi request) applies that frozen value verbatim;
/// recovery never catches the frozen configuration up to today's policy.
#[tokio::test]
async fn restart_applies_frozen_archive_resources_without_policy_catch_up() {
    let (runtime, params, state, server) = resume_tests::fixture(None).await;
    state.lock().expect("state").allow_writes = true;
    {
        let mut state = state.lock().expect("state");
        state.sts["spec"]["template"]["spec"]["containers"][0]["resources"] = serde_json::json!({
            "limits": {"cpu": "1500m", "memory": "6Gi"},
            "requests": {"cpu": "300m", "memory": "64Mi"}
        });
    }
    let mut source = captured(&runtime, &params).await;
    source.restart_image = Some("paired-runtime:new".into());
    let template = runtime
        .archive_builder_restart(&source)
        .await
        .expect("archive")
        .expect("K8s archive");
    {
        // Doctor the frozen candidate back to the pre-upgrade request so the
        // archive looks exactly like one written by the previous release.
        use base64::Engine as _;
        let mut state = state.lock().expect("state");
        let payload = state.archives[&template.archive.name]["data"]["template"]
            .as_str()
            .expect("archive payload")
            .to_owned();
        let mut archive: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(payload)
                .expect("base64"),
        )
        .expect("archive json");
        archive["candidate_pod_spec"]["containers"][0]["resources"]["requests"]["memory"] =
            "64Mi".into();
        state
            .archives
            .get_mut(&template.archive.name)
            .expect("archive")["data"]["template"] = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&archive).expect("re-encode"))
            .into();
    }
    assert!(
        runtime
            .apply_builder_control(&source, false)
            .await
            .expect("actual stop")
            .is_none()
    );
    let mut stopped = captured(&runtime, &params).await;
    stopped.restart_image = source.restart_image.clone();
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.start_builder_control_with_restart(&stopped, &template),
    )
    .await
    .expect("start budget");
    assert_eq!(
        started.expect("started").expect("ready").container_id,
        "pod-updated"
    );
    let state = state.lock().expect("state");
    let resources = &state.puts[0]["spec"]["template"]["spec"]["containers"][0]["resources"];
    assert_eq!(
        resources["requests"]["memory"], "64Mi",
        "the frozen archive value wins; a later policy never rewrites it"
    );
    assert_eq!(resources["limits"]["memory"], "6Gi");
    server.abort();
}

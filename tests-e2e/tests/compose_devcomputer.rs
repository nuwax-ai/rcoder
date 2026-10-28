//! Explicit Main DevComputer: controlled real ACP matrix and one real-model smoke.
use rcoder_e2e::common::{ChatRoute, report::JsonlReporter, require_context_or_skip};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};

#[tokio::test]
async fn devcomputer_main_protocol_contract() {
    if !require_context_or_skip() {
        return;
    }
    let route = ChatRoute::DevComputer;
    let report = JsonlReporter::begin(
        "devcomputer_main_protocol_contract",
        "compose",
        json!({"chat_path":route.chat_path(), "llm":false, "fixture":"controlled actual ACP process"}),
    );
    let result = (|| -> Result<(), String> {
        let root = PathBuf::from(
            std::env::var_os("E2E_REPORT_DIR").ok_or("strict report directory is required")?,
        );
        let python = std::env::var_os("E2E_PROTOCOL_PYTHON").unwrap_or_else(|| "python3".into());
        let status = Command::new(python)
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tools/devcomputer_protocol_contract.py"),
            )
            .args([
                "--chat-path",
                route.chat_path(),
                "--progress-prefix",
                route.progress_prefix(),
            ])
            .status()
            .map_err(|error| error.to_string())?;
        report.assert_hard(
            "devcomputer protocol child completed",
            status.success(),
            status.to_string(),
        );
        let bytes = std::fs::read(root.join("devcomputer-protocol/result.json"))
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        for check in value["checks"]
            .as_array()
            .ok_or("protocol checks missing")?
        {
            let name = check["name"]
                .as_str()
                .ok_or("protocol check name missing")?;
            report.assert_hard(
                name,
                check["ok"].as_bool() == Some(true),
                check["detail"].to_string(),
            );
        }
        report.assert_hard(
            "devcomputer original result explicit",
            value["success"] == true && value["fast_before_first_subscribe_covered"] == false
                && value["cleanup"]["captured_container_removed"] == true
                && value["cleanup"]["volumes_removed"] == false,
            json!({"error":value["error"], "cleanup_error":value["cleanup_error"],
                   "fast_before_first_subscribe_covered":value["fast_before_first_subscribe_covered"]}).to_string(),
        );
        Ok(())
    })();
    if let Err(error) = result {
        report.assert_hard("devcomputer protocol evidence readable", false, error);
    }
    assert!(report.finish(), "DevComputer real protocol contract failed");
}

/// Reality check beside the no-model protocol matrix. No sleep or second POST
/// can hide an unfinished first request or a fast-completion delivery gap.
#[tokio::test]
async fn devcomputer_main_real_model_smoke() {
    use rcoder_e2e::common::{
        Backend, Env, TestUserGuard, chat_reported_route, sse, sse_terminal_barrier,
    };
    use std::time::{Duration, Instant};
    let Some((env, report)) =
        Env::compose_or_skip("devcomputer_main_real_model_smoke", "openai").await
    else {
        return;
    };
    let user = env.scoped_user("devcomputer-real");
    let _guard = TestUserGuard::new(&env, &user);
    let nonce = format!("DEVC_SMOKE_{}", uuid::Uuid::new_v4().simple());
    let rid = format!("{}-devcomputer-real", env.run_tag);
    let request = env.base_payload(
        Backend::Openai,
        &format!("Reply with exactly this text: {nonce}"),
        &rid,
        &user,
    );
    report.assert_hard(
        "devcomputer real model omits explicit reload",
        request
            .agent_config
            .as_ref()
            .and_then(|config| config.auto_reload.as_ref())
            .is_none(),
        "wrapper must supply the default".into(),
    );
    let accepted = chat_reported_route(
        &env,
        &report,
        "original",
        &env.rcoder,
        &request,
        ChatRoute::DevComputer,
    )
    .await;
    report.assert_hard(
        "devcomputer direct real model accepted",
        accepted.is_ok(),
        format!("{accepted:?}"),
    );
    if let Ok(response) = accepted {
        let sid = response.session_id;
        report.assert_hard(
            "devcomputer direct real model actual SID",
            shared_types::SessionHint::classify(Some(&sid))
                .as_session_id()
                .is_some(),
            sid.clone(),
        );
        let phase = "observe_original_exact_terminal";
        // /computer/progress is the documented shared alias for DevComputer.
        // The no-model case also exercises /devcomputer/progress explicitly.
        report.subscribe_begin(phase, &env.rcoder, None);
        let observed = sse_terminal_barrier::observe(
            &env.sse_http,
            &env.rcoder,
            &sid,
            &rid,
            Instant::now() + Duration::from_secs(60),
            |event| {
                report.sse_event(phase, event.seq, &event.event, &event.data, event.t_ms);
            },
        )
        .await;
        report.subscribe_end(
            phase,
            &observed.ended_reason,
            observed.events.len(),
            &sse::ids_of(&observed.events),
            sse::type_counts(&observed.events),
            &sse::chunks_text(&observed.events),
        );
        report.assert_hard(
            "devcomputer direct real model exact terminal",
            observed.terminal_seq.is_some(),
            format!("original RID {rid}, reason {}", observed.ended_reason),
        );
        report.assert_hard(
            "devcomputer direct real model unique marker",
            sse::chunks_text(&observed.events).trim() == nonce,
            sse::chunks_text(&observed.events),
        );
        report.assert_hard(
            "devcomputer direct real model sequenced events",
            sse::monotonic_unique(&sse::ids_of(&observed.events))
                && observed.terminal_seq.is_some(),
            format!("terminal {:?}", observed.terminal_seq),
        );
    }
    assert!(
        report.finish(),
        "DevComputer real model original request failed"
    );
}

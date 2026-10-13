use super::*;
use axum::response::IntoResponse as _;

async fn project(root: &Path, package: serde_json::Value) {
    tokio::fs::create_dir_all(root.join("node_modules/.bin"))
        .await
        .unwrap();
    tokio::fs::write(root.join("package.json"), package.to_string())
        .await
        .unwrap();
}

#[tokio::test]
async fn dev_typecheck_priority_prefers_script_then_local_turbo_then_vue_tsc_then_tsc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    project(
        root,
        serde_json::json!({"scripts":{"typecheck":"custom-check"}}),
    )
    .await;
    for file in [
        "turbo.json",
        "tsconfig.json",
        "node_modules/.bin/turbo",
        "node_modules/.bin/vue-tsc",
        "node_modules/.bin/tsc",
    ] {
        tokio::fs::write(root.join(file), "{}").await.unwrap();
    }
    assert_eq!(choose(root).await.unwrap().command(), "pnpm run typecheck");
    tokio::fs::write(root.join("package.json"), "{}")
        .await
        .unwrap();
    assert_eq!(
        choose(root).await.unwrap().command(),
        "pnpm exec turbo run typecheck"
    );
    tokio::fs::remove_file(root.join("node_modules/.bin/turbo"))
        .await
        .unwrap();
    assert_eq!(
        choose(root).await.unwrap().command(),
        "pnpm exec vue-tsc --noEmit"
    );
    tokio::fs::remove_file(root.join("node_modules/.bin/vue-tsc"))
        .await
        .unwrap();
    assert_eq!(
        choose(root).await.unwrap().command(),
        "pnpm exec tsc --noEmit"
    );
    tokio::fs::remove_file(root.join("tsconfig.json"))
        .await
        .unwrap();
    assert!(choose(root).await.is_none());
}

#[tokio::test]
async fn dev_typecheck_no_package_or_typescript_project_skips_without_a_command() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let main = root.join("main.log");
    let temp = root.join("temp.log");
    run(root, "skip", &main, &temp).await.unwrap();
    project(root, serde_json::json!({"scripts":{"dev":"vite"}})).await;
    run(root, "skip", &main, &temp).await.unwrap();
    assert!(!main.exists() && !temp.exists());
    tokio::fs::write(root.join("package.json"), "{bad json")
        .await
        .unwrap();
    run(root, "skip", &main, &temp).await.unwrap();
}

#[cfg(unix)]
async fn pnpm_fixture(root: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    project(
        root,
        serde_json::json!({"scripts":{"typecheck":"custom-check"}}),
    )
    .await;
    let program = root.join("node_modules/.bin/pnpm");
    tokio::fs::write(
        &program,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> actual-arguments.log\n{script}\n"),
    )
    .await
    .unwrap();
    tokio::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .await
        .unwrap();
    program
}
async fn response(error: AppError) -> (axum::http::StatusCode, serde_json::Value) {
    let response = error.into_response();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_real_success_and_non_ts_failure_pass_with_both_logs() {
    for script in [
        "echo passed; exit 0",
        "echo environment-output; echo task-not-found >&2; exit 2",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        pnpm_fixture(root, script).await;
        let main = root.join("main.log");
        let temp = root.join("temp.log");
        run(root, "success", &main, &temp).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(root.join("actual-arguments.log"))
                .await
                .unwrap(),
            "run typecheck\n"
        );
        for log in [&main, &temp] {
            let text = tokio::fs::read_to_string(log).await.unwrap();
            assert!(text.contains("dev 启动前类型检查"));
            assert!(text.contains(if script.contains("exit 0") {
                "passed"
            } else {
                "task-not-found"
            }));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_real_stdout_and_stderr_ts_errors_are_business_400_with_original_details() {
    for stream in ["", " >&2"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        pnpm_fixture(root, &format!("echo 'src/App.ts(1,2): error TS2322: Type string is not assignable'{stream}; echo other-stream >&2; exit 2")).await;
        let error = run(
            root,
            "invalid",
            &root.join("main.log"),
            &root.join("temp.log"),
        )
        .await
        .unwrap_err();
        let (status, body) = response(error).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "TYPECHECK_FAILED");
        assert_eq!(body["error"]["type"], "BUSINESS_ERROR");
        assert_eq!(body["error"]["details"]["code"], "TYPECHECK_FAILED");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("error TS2322") && message.contains("other-stream"),
            "{body}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_timeout_passes_only_after_the_real_process_tree_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    pnpm_fixture(root, "sleep 30 & child=$!; echo $child > descendant.pid; echo 'error TS9999: before timeout'; wait $child").await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        run_with_timeout(
            root,
            "timeout",
            &root.join("main.log"),
            &root.join("temp.log"),
            1,
        ),
    )
    .await
    .unwrap();
    result.unwrap();
    let pid: u32 = tokio::fs::read_to_string(root.join("descendant.pid"))
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !process_utils::process_exists(pid).unwrap(),
        "typecheck descendant must be confirmed gone before pass-through"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_actual_cancel_and_current_journal_failure_do_not_pass_through() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    pnpm_fixture(root, "echo started > started; sleep 30").await;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = process_utils::command_context::CommandContext {
        identity: process_utils::command_context::WorkIdentity {
            task_id: Some("typecheck-test".into()),
            app_id: None,
        },
        cleanup_pending: Default::default(),
        journal_root: None,
        cancellation: cancellation.clone(),
    };
    let main_log = root.join("main.log");
    let temp_log = root.join("temp.log");
    let (result, _) = tokio::join!(
        context.scope(run(root, "cancel", &main_log, &temp_log)),
        async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !tokio::fs::try_exists(root.join("started")).await.unwrap() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            cancellation.cancel();
        }
    );
    assert!(
        result.is_err(),
        "actual cancellation cannot be an environment pass-through"
    );
    let journal = root.join("invalid-journal");
    tokio::fs::write(&journal, "current file blocks required durable record")
        .await
        .unwrap();
    let context = process_utils::command_context::CommandContext {
        identity: process_utils::command_context::WorkIdentity {
            task_id: Some("typecheck-persist".into()),
            app_id: None,
        },
        cleanup_pending: Default::default(),
        journal_root: Some(journal),
        cancellation: Default::default(),
    };
    let result = context
        .scope(run(
            root,
            "persist",
            &root.join("main-2.log"),
            &root.join("temp-2.log"),
        ))
        .await;
    assert!(
        result.is_err(),
        "current command intent persistence must succeed before any pass-through"
    );
    assert_eq!(
        tokio::fs::read_to_string(root.join("actual-arguments.log"))
            .await
            .unwrap()
            .lines()
            .count(),
        1,
        "persistence failure starts no new command"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_build_nonzero_is_business_400_with_both_streams_and_bounded_raw() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let program = pnpm_fixture(
        root,
        "echo stdout-build-diagnostic; echo stderr-build-diagnostic >&2; exit 3",
    )
    .await;
    let error = run_build_script(
        program.to_str().unwrap(),
        &["run", "build"],
        root,
        &root.join("main.log"),
        &root.join("temp.log"),
        5,
    )
    .await
    .unwrap_err();
    let (status, body) = response(error).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UNKNOWN_ERROR");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("stdout-build-diagnostic") && message.contains("stderr-build-diagnostic"),
        "{body}"
    );
    let mut capture = CapturedOutput::default();
    capture.line(&"中".repeat(7000));
    let output = capture.text();
    assert_eq!(
        output.split("\n...").next().unwrap().chars().count(),
        MAX_OUTPUT_CHARS
    );
    assert!(output.contains("output truncated"));
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_missing_pnpm_skips_without_starting_a_process() {
    const CHILD: &str = "RCODER_TYPECHECK_NO_TOOL_CHILD";
    const TEST: &str = "service::dev_typecheck::tests::dev_typecheck_missing_pnpm_skips_without_starting_a_process";
    if std::env::var_os(CHILD).is_some() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        project(
            root,
            serde_json::json!({"scripts":{"typecheck":"custom-check"}}),
        )
        .await;
        let main = root.join("main.log");
        let temp = root.join("temp.log");
        assert!(pnpm_program(root).await.is_none());
        run(root, "no-tool", &main, &temp).await.unwrap();
        assert!(!main.exists() && !temp.exists());
        println!("TYPECHECK_NO_TOOL_SKIPPED_REAL");
        return;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("PATH", "")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("TYPECHECK_NO_TOOL_SKIPPED_REAL"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_real_config_only_5057_and_18003_failures_pass_and_preserve_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    pnpm_fixture(root, "echo 'error TS5057: Cannot find a tsconfig.json file'; echo 'error TS18003: No inputs were found' >&2; exit 2").await;
    let main = root.join("main.log");
    let temp = root.join("temp.log");
    run(root, "config", &main, &temp).await.unwrap();
    for path in [&main, &temp] {
        let text = tokio::fs::read_to_string(path).await.unwrap();
        assert!(
            text.contains("TS5057") && text.contains("TS18003"),
            "{text}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dev_typecheck_real_config_diagnostics_mixed_with_source_2322_still_return_400() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    pnpm_fixture(root, "echo 'error TS5057: Cannot find a tsconfig.json file'; echo 'error TS18003: No inputs were found'; echo 'src/App.ts(1,2): error TS2322: genuine source mismatch' >&2; exit 2").await;
    let error = run(
        root,
        "mixed",
        &root.join("main.log"),
        &root.join("temp.log"),
    )
    .await
    .unwrap_err();
    let (status, body) = response(error).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "TYPECHECK_FAILED");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("TS5057") && message.contains("TS18003") && message.contains("TS2322"),
        "{body}"
    );
    assert!(has_ts_diagnostics(
        "error TS5075: subtype constraint source error"
    ));
    assert!(has_ts_diagnostics("error TS18047: value is possibly null"));
}

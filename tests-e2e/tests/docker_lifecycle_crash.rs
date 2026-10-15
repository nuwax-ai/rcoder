//! Real rcoder Docker SIGKILL and Docker crash restart quarantine contracts.
use rcoder_e2e::common::report::JsonlReporter;

fn observer_beside_test(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    let directory = executable.parent()?;
    let frozen = directory.join("userapp-db-observer");
    if frozen.is_file() {
        return Some(frozen);
    }
    // Unfrozen Cargo integration tests live in target/<profile>/deps. A frozen
    // run/bin must never search outside its own binary snapshot.
    if directory.file_name()? != "deps" {
        return None;
    }
    let cargo_artifact = directory.parent()?.join("userapp-db-observer");
    cargo_artifact.is_file().then_some(cargo_artifact)
}

#[test]
fn docker_runtime_crash_recovery_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "docker_runtime_crash_recovery_contract",
        "docker-crash",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/docker_crash_contract.py"
        ))
        .status();
    report.assert_hard(
        "Docker crash contract process completed",
        result.is_ok_and(|s| s.success()),
        "see docker-crash artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("docker-crash/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Docker crash assertions file"))
            .expect("Docker crash assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Docker crash lifecycle contract failed");
}

/// Graceful shutdown is a separate contract from SIGKILL restart quarantine.
#[test]
fn docker_runtime_sigterm_drain_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "docker_runtime_sigterm_drain_contract",
        "docker-shutdown",
        serde_json::json!({"isolated": true}),
    );
    let mut command = std::process::Command::new("python3");
    command.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tools/docker_shutdown_contract.py"
    ));
    // Prefer the case's pre-frozen observer or an explicit build artifact. This
    // test never spawns Cargo and never substitutes SQLite for the Turso reader.
    let report_dir =
        std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").expect("E2E report directory"));
    if std::env::var_os("E2E_USERAPP_OBSERVER_BINARY").is_none()
        && !report_dir.join("observer/userapp-db-observer").is_file()
    {
        let executable = std::env::current_exe().expect("E2E test executable");
        let observer = observer_beside_test(&executable)
            .expect("observer in frozen run/bin or existing Cargo profile artifact");
        command.arg("--observer").arg(observer);
    }
    let result = command.status();
    report.assert_hard(
        "Docker SIGTERM contract process completed",
        result.is_ok_and(|status| status.success()),
        "see docker-shutdown artifacts".into(),
    );
    let path = report_dir.join("docker-shutdown/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Docker SIGTERM assertions file"))
            .expect("Docker SIGTERM assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Docker SIGTERM drain contract failed");
}

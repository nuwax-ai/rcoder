//! Owned Docker cleanup with evidence captured before deletion.
use std::{path::PathBuf, process::Command};

/// Record the immutable identity returned by a newly created test project.
/// Call immediately after the create response, before any other lifecycle work.
pub fn register_created_container(name: &str) -> Result<(), String> {
    let Some(root) = std::env::var_os("E2E_REPORT_DIR") else {
        return Ok(());
    };
    let identity = Command::new("docker")
        .args([
            "inspect",
            "--format",
            r#"{"id":{{json .Id}},"image":{{json .Image}},"state":{{json .State}}}"#,
            name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !identity.status.success() {
        return Err("register created container identity failed".into());
    }
    let directory = PathBuf::from(root).join("resources");
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let mut record: serde_json::Value =
        serde_json::from_slice(&identity.stdout).map_err(|e| e.to_string())?;
    if let Ok(before) = std::env::var("E2E_EXISTING_CONTAINER_IDS") {
        let existing: Vec<String> = serde_json::from_str(&before).map_err(|e| e.to_string())?;
        if record["id"]
            .as_str()
            .is_some_and(|id| existing.iter().any(|old| old == id))
        {
            return Err(
                "container existed before this run; refusing ownership registration".into(),
            );
        }
    }
    record["name"] = name.into();
    record["case_id"] = std::env::var("E2E_CASE_ID").ok().into();
    std::fs::write(
        directory.join(format!("{name}-ownership.json")),
        record.to_string(),
    )
    .map_err(|e| e.to_string())
}

pub fn cleanup_container(name: &str) -> Result<(), String> {
    let outcome = cleanup_inner(name, true);
    if let Some(root) = std::env::var_os("E2E_REPORT_DIR") {
        let directory = PathBuf::from(root).join("resources");
        let record = serde_json::json!({"name": name, "ok": outcome.is_ok(), "error": outcome.as_ref().err()});
        if let Err(error) = std::fs::create_dir_all(&directory).and_then(|()| {
            std::fs::write(
                directory.join(format!("{name}-cleanup.json")),
                record.to_string(),
            )
        }) {
            return Err(format!("write cleanup evidence: {error}"));
        }
    }
    outcome
}

pub fn capture_container(name: &str) -> Result<(), String> {
    cleanup_inner(name, false)
}

fn cleanup_inner(name: &str, delete: bool) -> Result<(), String> {
    let receipt = std::env::var_os("E2E_REPORT_DIR")
        .and_then(|root| {
            std::fs::read(
                PathBuf::from(root)
                    .join("resources")
                    .join(format!("{name}-ownership.json")),
            )
            .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if let Ok(case) = std::env::var("E2E_CASE_ID")
        && !case.get(..10).is_some_and(|prefix| name.contains(prefix))
        && !receipt
            .as_ref()
            .is_some_and(|r| r["name"] == name && r["case_id"] == case)
    {
        return Err(
            "container does not belong to this case namespace or registered receipt".into(),
        );
    }
    let listing = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--no-trunc",
            "--format",
            "{{.ID}} {{.Names}}",
            "--filter",
            &format!("name=^/{name}$"),
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !listing.status.success() {
        return Err("Docker inventory failed".into());
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    let found = listing.lines().find_map(|line| {
        let (id, actual) = line.split_once(' ')?;
        (actual == name).then_some(id.to_owned())
    });
    let Some(id) = found else {
        if !delete {
            return Err("container missing during evidence capture".into());
        }
        return match name.strip_prefix("rcoder-app-builder-") {
            Some(app_id) => purge_app(app_id),
            None => Ok(()),
        };
    };
    if let Ok(before) = std::env::var("E2E_EXISTING_CONTAINER_IDS") {
        let existing: Vec<String> = serde_json::from_str(&before).map_err(|e| e.to_string())?;
        if existing.contains(&id) {
            return Err("container existed before this run; refusing cleanup".into());
        }
    }
    if receipt.as_ref().is_some_and(|r| r["id"] != id) {
        return Err("container identity changed since registration; refusing cleanup".into());
    }
    let diagnostics = (|| -> Result<(), String> {
        if let Some(root) = std::env::var_os("E2E_REPORT_DIR") {
            let directory = PathBuf::from(root).join("resources");
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let identity = Command::new("docker")
                .args(["inspect", "--format", "{{.Id}} {{.Name}} {{.Image}}", &id])
                .output()
                .map_err(|e| e.to_string())?;
            if !identity.status.success() {
                return Err("capture container identity failed".into());
            }
            std::fs::write(
                directory.join(format!("{name}-identity.txt")),
                identity.stdout,
            )
            .map_err(|e| e.to_string())?;
            let logs = Command::new("docker")
                .args(["logs", "--tail", "500", &id])
                .output()
                .map_err(|e| e.to_string())?;
            let runtime_logs = Command::new("docker")
                .args(["exec", &id, "sh", "-c", "tail -n 100 /home/user/logs/app-cli.err.log /home/user/logs/app-cli.out.log /home/user/logs/app-cli.log.* /home/user/logs/services/*.log 2>/dev/null; wget -qO- http://127.0.0.1:3010/v1/deploy/status"])
                .output().map_err(|e| e.to_string())?;
            let environment = Command::new("docker")
                .args(["inspect", "--format", "{{json .Config.Env}}", &id])
                .output()
                .map_err(|e| e.to_string())?;
            let values: Vec<String> = serde_json::from_slice(&environment.stdout)
                .map_err(|e| format!("read redaction inputs: {e}"))?;
            let mut text = format!(
                "{}{}{}{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr),
                String::from_utf8_lossy(&runtime_logs.stdout),
                String::from_utf8_lossy(&runtime_logs.stderr)
            );
            for pair in values {
                if let Some((key, value)) = pair.split_once('=') {
                    let key = key.to_ascii_uppercase();
                    if value.len() >= 4
                        && ["TOKEN", "KEY", "PASSWORD", "SECRET"]
                            .iter()
                            .any(|part| key.contains(part))
                    {
                        text = text.replace(value, "[REDACTED]");
                    }
                }
            }
            let api_key = super::Env::load().api_key;
            if !api_key.is_empty() {
                text = text.replace(&api_key, "[REDACTED]");
            }
            std::fs::write(directory.join(format!("{name}-logs.txt")), text)
                .map_err(|e| e.to_string())?;
            let state = Command::new("docker")
                .args(["inspect", "--format", "{{json .State}}", &id])
                .output()
                .map_err(|e| e.to_string())?;
            std::fs::write(directory.join(format!("{name}-state.json")), state.stdout)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if !delete {
        return diagnostics;
    }
    if let Some(app_id) = name.strip_prefix("rcoder-app-builder-") {
        return purge_app(app_id).and(diagnostics);
    }
    let deleted = Command::new("docker")
        .args(["rm", "-f", &id])
        .output()
        .map_err(|e| e.to_string())?;
    if !deleted.status.success() {
        return Err("owned container deletion failed".into());
    }
    diagnostics
}

fn purge_app(app_id: &str) -> Result<(), String> {
    // Exercise the formal UserApp purge contract so test-owned data and
    // metadata are reclaimed as well as the container. Agent PVCs are unrelated.
    let endpoint = format!(
        "{}/api/v1/userapp/{app_id}/delete/app",
        super::Env::load().rcoder
    );
    let response = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "60",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data",
            "{}",
            &endpoint,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    classify_purge_response(
        response.status.success(),
        response.status.code(),
        &response.stdout,
        &response.stderr,
        &super::Env::load().api_key,
    )
}

fn classify_purge_response(
    success: bool,
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    api_key: &str,
) -> Result<(), String> {
    let redact = |text: &str| {
        if api_key.is_empty() {
            text.to_owned()
        } else {
            text.replace(api_key, "[REDACTED]")
        }
    };
    if !success {
        return Err(serde_json::json!({
            "category": "transport",
            "transport": if exit_code == Some(28) { "timeout" } else { "curl_failure" },
            "exit_code": exit_code,
            "stderr": redact(&String::from_utf8_lossy(stderr)),
        })
        .to_string());
    }
    let body: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("invalid purge envelope: {error}"))?;
    if body["code"] != "0000" {
        return Err(serde_json::json!({
            "category": "business",
            "code": redact(body["code"].as_str().unwrap_or("<missing>")),
            "message": redact(body["message"].as_str().unwrap_or("<missing>")),
        })
        .to_string());
    }
    Ok(())
}

#[cfg(test)]
mod purge_diagnostics_tests {
    use super::classify_purge_response;

    #[test]
    fn purge_timeout_preserves_exit_code_and_redacts_stderr() {
        let error =
            classify_purge_response(false, Some(28), b"", b"timeout secret-key", "secret-key")
                .expect_err("transport error");
        let value: serde_json::Value = serde_json::from_str(&error).expect("diagnostic JSON");
        assert_eq!(value["transport"], "timeout");
        assert_eq!(value["exit_code"], 28);
        assert_eq!(value["stderr"], "timeout [REDACTED]");
    }

    #[test]
    fn purge_rejection_keeps_business_identity_and_redacts_message() {
        let error = classify_purge_response(
            true,
            Some(0),
            br#"{"code":"ERR_CONFLICT","message":"pending secret-key"}"#,
            b"",
            "secret-key",
        )
        .expect_err("business rejection");
        let value: serde_json::Value = serde_json::from_str(&error).expect("diagnostic JSON");
        assert_eq!(value["code"], "ERR_CONFLICT");
        assert_eq!(value["message"], "pending [REDACTED]");
    }

    #[test]
    fn purge_success_definition_is_unchanged() {
        assert!(classify_purge_response(true, Some(0), br#"{"code":"0000"}"#, b"", "").is_ok());
        assert!(
            classify_purge_response(true, Some(0), br#"{"code":"ERR_CONFLICT"}"#, b"", "").is_err()
        );
    }
}

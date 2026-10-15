//! Owned Docker cleanup with evidence captured before deletion.
use std::{path::PathBuf, process::Command};

/// Record the immutable identity returned by a newly created test project.
/// Call immediately after the create response, before any other lifecycle work.
pub fn register_created_container(name: &str) -> Result<(), String> {
    register_container_identity(name, None, None)
}

fn register_container_identity(
    name: &str,
    expected_owner: Option<&str>,
    expected_previous: Option<&str>,
) -> Result<(), String> {
    let Some(root) = std::env::var_os("E2E_REPORT_DIR") else {
        return Ok(());
    };
    let identity = Command::new("docker")
        .args([
            "inspect",
            "--format",
            r#"{"id":{{json .Id}},"image":{{json .Image}},"state":{{json .State}},"service_type":{{json (index .Config.Labels "service-type")}},"app_id":{{json (index .Config.Labels "rcoder.io/application-id")}},"user_id":{{json (index .Config.Labels "rcoder.io/owner-id")}},"lifecycle_id":{{json (index .Config.Labels "rcoder.io/lifecycle-id")}}}"#,
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
    if let Some(owner) = expected_owner {
        let instance = name
            .strip_prefix("rcoder-app-builder-")
            .ok_or("invalid builder name")?;
        // record.app_id（docker label application-id）与容器名同为复合实例串
        if record["app_id"] != instance
            || record["user_id"] != owner
            || record["service_type"] != shared_types::ServiceType::UserappBuilder.to_string()
            || !record["lifecycle_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        {
            return Err("builder physical labels do not match the requested identity".into());
        }
    }
    let receipt_path = directory.join(format!("{name}-ownership.json"));
    if expected_previous.is_some() && !receipt_path.exists() {
        return Err("replacement requires an existing creation receipt".into());
    }
    if expected_owner.is_some() && receipt_path.exists() {
        let previous: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&receipt_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if previous["case_id"].as_str() != std::env::var("E2E_CASE_ID").ok().as_deref() {
            return Err("creation receipt belongs to another test case".into());
        }
        if let Some(expected) = expected_previous {
            let removal: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join(format!("{name}-recreation-removal.json")))
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            prove_builder_replacement(&previous, &record, &removal, expected)?;
            let mut predecessors = previous["predecessors"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            predecessors.push(serde_json::json!({"id": previous["id"], "lifecycle_id": previous["lifecycle_id"], "user_id": previous["user_id"]}));
            record["predecessors"] = predecessors.into();
        } else {
            if previous["id"] != record["id"] || previous["lifecycle_id"] != record["lifecycle_id"]
            {
                return Err(
                    "registered resource identity changed; refusing receipt replacement".into(),
                );
            }
            if let Some(predecessors) = previous.get("predecessors") {
                record["predecessors"] = predecessors.clone();
            }
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

/// Observe a successful or uncertain builder creation immediately. Only the
/// current case's namespace and authoritative owner/family labels can authorize
/// registration; absence after an unsuccessful HTTP response is not an error.
pub fn register_builder_attempt(app_id: &str, required: bool) -> Result<(), String> {
    let case = std::env::var("E2E_CASE_ID").map_err(|_| "strict case identity is required")?;
    if !case.get(..10).is_some_and(|prefix| app_id.contains(prefix)) {
        return Err("builder app ID is outside the current case namespace".into());
    }
    let query = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            // 应用共享（用户绑定移除）：application-id 标签值 = 纯 app_id
            &format!("label=rcoder.io/application-id={app_id}"),
            "--filter",
            &format!(
                "label=service-type={}",
                shared_types::ServiceType::UserappBuilder
            ),
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !query.status.success() {
        return Err("builder ownership query failed".into());
    }
    let output = String::from_utf8(query.stdout).map_err(|error| error.to_string())?;
    let count = output.split_whitespace().count();
    if count == 0 && !required {
        return Ok(());
    }
    if count != 1 {
        return Err("expected exactly one owned builder after creation".into());
    }
    register_container_identity(&format!("rcoder-app-builder-{app_id}"), None, None)
}

fn prove_builder_replacement(
    previous: &serde_json::Value,
    current: &serde_json::Value,
    removal: &serde_json::Value,
    expected_previous: &str,
) -> Result<(), String> {
    if previous["id"] != expected_previous
        || current["id"] == expected_previous
        || current["id"].as_str().is_none_or(str::is_empty)
        || previous["app_id"] != current["app_id"]
        || previous["user_id"] != current["user_id"]
        || previous["lifecycle_id"] != current["lifecycle_id"]
        || previous["lifecycle_id"].as_str().is_none_or(str::is_empty)
        || previous["service_type"] != "user-app-builder"
        || current["service_type"] != "user-app-builder"
        || removal["id"] != expected_previous
        || removal["absent"] != true
        || removal["case_id"] != previous["case_id"]
    {
        return Err("builder replacement lacks matching removal and lifecycle evidence".into());
    }
    Ok(())
}

fn require_container_absent(id: &str) -> Result<(), String> {
    let result = Command::new("docker")
        .args(["ps", "-aq", "--no-trunc", "--filter", &format!("id={id}")])
        .output()
        .map_err(|error| error.to_string())?;
    if !result.status.success() || !result.stdout.is_empty() {
        return Err("old builder is still present or its absence could not be verified".into());
    }
    Ok(())
}

/// Deliberate test fault: remove only the registered physical builder, retaining
/// application identity and data. This is not full application cleanup.
pub fn remove_builder_for_recreation(app_id: &str, _user_id: &str, id: &str) -> Result<(), String> {
    let root =
        PathBuf::from(std::env::var_os("E2E_REPORT_DIR").ok_or("strict report context required")?)
            .join("resources");
    let name = format!("rcoder-app-builder-{app_id}");
    let previous: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(format!("{name}-ownership.json")))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if previous["id"] != id
        || previous["app_id"] != app_id
        || previous["case_id"].as_str() != std::env::var("E2E_CASE_ID").ok().as_deref()
        || previous["service_type"] != "user-app-builder"
    {
        return Err("builder recreation fault does not own the captured physical container".into());
    }
    capture_container(&name)?;
    let removal = Command::new("docker")
        .args(["rm", "-f", id])
        .output()
        .map_err(|error| error.to_string())?;
    if !removal.status.success() {
        return Err("owned builder removal failed".into());
    }
    require_container_absent(id)?;
    std::fs::write(root.join(format!("{name}-recreation-removal.json")),
        serde_json::json!({"id": id, "absent": true, "case_id": previous["case_id"], "lifecycle_id": previous["lifecycle_id"]}).to_string()
    ).map_err(|error| error.to_string())
}

/// Record an explicitly observed replacement without losing predecessor proof.
pub fn register_builder_replacement(
    app_id: &str,
    // T1 后定位与 user 无关；参数保留以维持调用点占位语义（URL 段 "0"）
    _user_id: &str,
    previous_id: &str,
) -> Result<(), String> {
    require_container_absent(previous_id)?;
    register_container_identity(
        &format!("rcoder-app-builder-{app_id}"),
        None,
        Some(previous_id),
    )
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
            Some(app_id) => purge_app(app_id, receipt.as_ref()),
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
        // 系统自愈/重建会产生同应用新容器（同 app/lifecycle 标签）：校验当前
        // 容器标签仍与登记的应用身份一致则刷新回执继续清理；标签不符（真身份
        // 对调）依旧拒绝。T1 用户绑定移除后 owner-id 标签不再写入——身份锚点
        // 为 application-id + lifecycle-id（回执 user_id 恒 null，不再比对）。
        let labels = Command::new("docker")
            .args(["inspect", "--format", "{{json .Config.Labels}}", &id])
            .output()
            .map_err(|e| e.to_string())?;
        if !labels.status.success() {
            return Err("container identity changed; label verification failed".into());
        }
        let labels: serde_json::Value =
            serde_json::from_slice(&labels.stdout).map_err(|e| e.to_string())?;
        let matches_receipt = receipt.as_ref().is_some_and(|r| {
            labels["rcoder.io/application-id"].as_str() == r["app_id"].as_str()
                && r["lifecycle_id"].as_str().is_some_and(|lifecycle| {
                    labels["rcoder.io/lifecycle-id"].as_str() == Some(lifecycle)
                        && !lifecycle.is_empty()
                })
        });
        if !matches_receipt {
            return Err("container identity changed since registration; refusing cleanup".into());
        }
        let mut refreshed = receipt.clone().unwrap_or_default();
        refreshed["id"] = serde_json::json!(id);
        if let Some(root) = std::env::var_os("E2E_REPORT_DIR") {
            let path = PathBuf::from(root)
                .join("resources")
                .join(format!("{name}-ownership.json"));
            std::fs::write(
                &path,
                serde_json::to_string(&refreshed).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
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
                .args(["exec", &id, "sh", "-c", "tail -n 100 /home/user/logs/app-cli.err.log /home/user/logs/app-cli.out.log /home/user/logs/app-cli.log.* /home/user/logs/pg.out.log /home/user/logs/pg.err.log /home/user/logs/services/*.log 2>/dev/null; wget -qO- http://127.0.0.1:3010/v1/deploy/status"])
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
        return purge_app(app_id, receipt.as_ref()).and(diagnostics);
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

fn purge_body(app_id: &str, receipt: Option<&serde_json::Value>) -> Result<String, String> {
    let receipt = receipt.ok_or("application cleanup requires a creation identity receipt")?;
    if receipt["app_id"].as_str() != Some(app_id) {
        return Err("application receipt identity does not match cleanup target".into());
    }
    // 共享模型：user_id 不再是必填归属档（owner 标签/注册已退役）
    let lifecycle_id = receipt["lifecycle_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or("application receipt is missing its lifecycle identity")?;
    let case_id = receipt["case_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or("application receipt is missing its test case identity")?;
    Ok(serde_json::json!({
        "lifecycle_id": lifecycle_id,
        "request_id": format!("cleanup-{case_id}-{app_id}"),
    })
    .to_string())
}

fn purge_app(app_id: &str, receipt: Option<&serde_json::Value>) -> Result<(), String> {
    let body = purge_body(app_id, receipt)?;
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
            &body,
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

#[cfg(test)]
mod lifecycle_receipt_tests {
    use super::{prove_builder_replacement, purge_body};

    #[test]
    fn replacement_requires_old_absence_same_lifecycle_and_explicit_predecessor() {
        let previous = serde_json::json!({"id":"old", "app_id":"app", "user_id":"owner",
            "service_type":"user-app-builder", "lifecycle_id":"life", "case_id":"case"});
        let mut current = previous.clone();
        current["id"] = "new".into();
        let removal = serde_json::json!({"id":"old", "absent":true, "case_id":"case"});
        assert!(prove_builder_replacement(&previous, &current, &removal, "old").is_ok());
        assert!(prove_builder_replacement(&previous, &current, &removal, "different").is_err());
        for field in ["id", "app_id", "user_id", "lifecycle_id", "service_type"] {
            let mut wrong = current.clone();
            wrong[field] = if field == "id" { "old" } else { "different" }.into();
            assert!(
                prove_builder_replacement(&previous, &wrong, &removal, "old").is_err(),
                "{field}"
            );
        }
        for field in ["id", "absent", "case_id"] {
            let mut missing = removal.clone();
            missing.as_object_mut().expect("object").remove(field);
            assert!(
                prove_builder_replacement(&previous, &current, &missing, "old").is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn cleanup_uses_captured_owner_and_lifecycle_and_requires_complete_receipt() {
        let receipt = serde_json::json!({
            "app_id": "receipt-app", "user_id": "receipt-owner",
            "lifecycle_id": "original-life", "case_id": "case-one"
        });
        let body: serde_json::Value =
            serde_json::from_str(&purge_body("receipt-app", Some(&receipt)).expect("body"))
                .expect("json");
        // 共享模型：user_id 不再是 purge body 字段（归属档退役）
        assert!(body.get("user_id").is_none());
        assert_eq!(body["lifecycle_id"], "original-life");
        assert_eq!(body["request_id"], "cleanup-case-one-receipt-app");
        assert!(purge_body("different-app", Some(&receipt)).is_err());
        assert!(purge_body("receipt-app", None).is_err());
        for field in ["lifecycle_id", "case_id"] {
            let mut incomplete = receipt.clone();
            incomplete.as_object_mut().expect("object").remove(field);
            assert!(purge_body("receipt-app", Some(&incomplete)).is_err());
        }
    }
}

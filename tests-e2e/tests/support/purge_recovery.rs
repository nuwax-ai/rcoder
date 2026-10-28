//! Purge remains destructive; old traffic cannot recreate data, fresh URL can.
use super::*;
use std::path::{Path, PathBuf};

pub(super) struct Captured {
    id: String,
    mounts: Vec<(PathBuf, String)>,
}

fn inspect(app: &str) -> Result<Value, String> {
    let output = std::process::Command::new("docker")
        .args(["inspect", &format!("rcoder-app-{app}")])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let rows: Vec<Value> =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    rows.into_iter()
        .next()
        .ok_or_else(|| "runtime inspect is empty".into())
}

fn observe(app: &str, seed_old_markers: bool) -> Result<Captured, String> {
    let case = std::env::var("E2E_CASE_ID").map_err(|error| error.to_string())?;
    if !case.get(..10).is_some_and(|prefix| app.contains(prefix)) {
        return Err("purge fixture app is outside the current case namespace".into());
    }
    let row = inspect(app)?;
    if row["Config"]["Labels"][shared_types::USERAPP_DOCKER_APP_ID_LABEL] != app
        || row["Config"]["Labels"]["service-type"] != shared_types::ServiceType::Userapp.to_string()
    {
        return Err("purge fixture runtime ownership changed".into());
    }
    let mut mounts = row["Mounts"]
        .as_array()
        .ok_or("runtime mounts missing")?
        .iter()
        .filter(|mount| mount["Type"] == "bind")
        .map(|mount| {
            Ok((
                PathBuf::from(mount["Source"].as_str().ok_or("mount source missing")?),
                mount["Destination"]
                    .as_str()
                    .ok_or("mount destination missing")?
                    .to_owned(),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if mounts.len() != 4 {
        return Err("purge fixture requires the actual four application binds".into());
    }
    mounts.sort_by(|left, right| left.1.cmp(&right.1));
    if seed_old_markers {
        let destinations: Vec<_> = mounts
            .iter()
            .map(|(_, target)| format!("{target}/e2e-purge-old-marker"))
            .collect();
        let output = std::process::Command::new("docker").args([
            "exec", "--user", "0", row["Id"].as_str().ok_or("runtime CID missing")?, "python3", "-c",
            "import json,pathlib,sys; [pathlib.Path(p).write_text(sys.argv[2]) for p in json.loads(sys.argv[1])]",
            &serde_json::to_string(&destinations).map_err(|error| error.to_string())?, app,
        ]).output().map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "seed owned purge data: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(Captured {
        id: row["Id"].as_str().ok_or("runtime CID missing")?.to_owned(),
        mounts,
    })
}

pub(super) fn capture(app: &str) -> Result<Captured, String> {
    observe(app, true)
}

fn no_runtime(app: &str) -> Result<bool, String> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &format!("label=rcoder.io/application-id={app}"),
            "--filter",
            "label=service-type=user-app",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(output.stdout.is_empty())
}

fn paths_absent(captured: &Captured) -> Result<bool, std::io::Error> {
    for (source, _) in &captured.mounts {
        if source.try_exists()? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn old_traffic_stays_deleted(
    env: &Env,
    app: &str,
    user: &str,
    captured: &Captured,
) -> Result<Vec<Value>, String> {
    let mut observed = Vec::new();
    // Keep all six real requests: one refused request does not prove no wake.
    for _ in 0..6 {
        let response = env
            .http
            .get(format!(
                "{}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/",
                pingora_base()
            ))
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.map_err(|error| error.to_string())?;
        let refused = status == 502
            || (status == 409 && body["error"]["reason"] == shared_types::ERR_CONFLICT);
        observed.push(json!({"status":status,"body":body}));
        if !refused
            || !no_runtime(app)?
            || !paths_absent(captured).map_err(|error| error.to_string())?
        {
            return Err(format!(
                "old traffic recreated purged resources or lost its refusal: {observed:?}"
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(observed)
}

fn artifact_host() -> Result<String, String> {
    if let Ok(host) = std::env::var("E2E_ARTIFACT_HOST")
        && !host.trim().is_empty()
    {
        return Ok(host);
    }
    if !cfg!(target_os = "linux") {
        return Ok("host.docker.internal".into());
    }
    let controller = std::env::var("E2E_OWNER_CONTROLLER_ID").map_err(|_| {
        "set E2E_ARTIFACT_HOST to a host address reachable by Docker containers".to_owned()
    })?;
    let output = std::process::Command::new("docker")
        .args(["inspect", &controller])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("cannot observe the owned controller network".into());
    }
    let rows: Vec<Value> =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    let controller = rows.first().ok_or("controller inspect is empty")?;
    let networks = controller["NetworkSettings"]["Networks"]
        .as_object()
        .ok_or("controller networks missing")?;
    let gateways: std::collections::BTreeSet<_> = networks
        .values()
        .filter_map(|row| row["Gateway"].as_str())
        .filter(|value| !value.is_empty())
        .collect();
    if gateways.len() != 1 {
        return Err("set E2E_ARTIFACT_HOST for this multi-network controller".into());
    }
    Ok(gateways
        .into_iter()
        .next()
        .ok_or("controller gateway missing")?
        .to_owned())
}

struct ArtifactServer(tokio::task::JoinHandle<()>);
impl Drop for ArtifactServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn verify(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    captured: Captured,
    artifact: Option<(&str, &str)>,
) {
    let old = old_traffic_stays_deleted(env, app, user, &captured).await;
    report.assert_hard(
        "purged old traffic never recreates runtime or four data directories",
        old.is_ok(),
        format!("{old:?}"),
    );
    let Some((release, expected_sha)) = artifact.filter(|_| old.is_ok()) else {
        return;
    };
    let preparation = async {
        let path = Path::new(&std::env::var("E2E_REPORT_DIR").map_err(|error| error.to_string())?)
            .join("verified-artifact.zip");
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let actual_sha: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual_sha != expected_sha {
            return Err("fresh URL artifact changed after verification".to_owned());
        }
        let bytes = axum::body::Bytes::from(bytes);
        let route = format!("/fresh-{}", uuid::Uuid::new_v4().simple());
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|error| error.to_string())?;
        let url = format!(
            "http://{}:{}{route}",
            artifact_host()?,
            listener
                .local_addr()
                .map_err(|error| error.to_string())?
                .port()
        );
        let router = axum::Router::new().route(
            &route,
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
        let task = ArtifactServer(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                eprintln!("private artifact server failed: {error}");
            }
        }));
        Ok::<_, String>((url, task))
    }
    .await;
    report.assert_hard(
        "purge recovery fresh verified URL is prepared",
        preparation.is_ok(),
        preparation.as_ref().err().cloned().unwrap_or_default(),
    );
    let Ok((url, _server)) = preparation else {
        return;
    };
    let (status, body) = post_json(env, &format!("/api/v1/userapp/{app}/start"),
        json!({"user_id":user,"url":url,"release_id":release,"sha256":expected_sha,"request_id":format!("fresh-after-purge-{app}")})).await;
    let accepted = status.is_success() && http_ok(&body);
    report.assert_hard(
        "purge explicit fresh URL deployment is accepted",
        accepted,
        format!("{status}: {}", trunc(&body, 500)),
    );
    if !accepted {
        return;
    }
    let archived = (|| {
        let root = PathBuf::from(
            std::env::var_os("E2E_REPORT_DIR").ok_or("case report directory missing")?,
        )
        .join("resources");
        let archive = root.join(format!("pre-purge-{}", captured.id));
        std::fs::create_dir(&archive).map_err(|error| error.to_string())?;
        for entry in std::fs::read_dir(&root).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("rcoder-app-{app}-"))
            {
                std::fs::copy(entry.path(), archive.join(entry.file_name()))
                    .map_err(|error| error.to_string())?;
            }
        }
        rcoder_e2e::common::resources::register_created_container(&format!("rcoder-app-{app}"))?;
        rcoder_e2e::common::resources::capture_container(&format!("rcoder-app-{app}"))?;
        Ok::<_, String>(())
    })();
    report.assert_hard(
        "fresh purge deployment identity is recorded without losing old evidence",
        archived.is_ok(),
        archived.err().unwrap_or_default(),
    );
    let operation_id = body["data"]["operation_id"].as_str();
    let started = Instant::now();
    let mut ready = false;
    while started.elapsed() < ready_budget() {
        let remaining = ready_budget().saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let settled = if let Some(operation) = operation_id {
            match env
                .http
                .get(format!(
                    "{}/api/v1/userapp/{app}/operations/{operation}?user_id={user}",
                    env.rcoder
                ))
                .timeout(remaining.min(Duration::from_secs(10)))
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let observed: Value = response.json().await.unwrap_or(Value::Null);
                    report.diagnostic(
                        "fresh URL original operation",
                        &trunc(&observed, 2000),
                        &format!("HTTP {status}"),
                    );
                    status.is_success()
                        && http_ok(&observed)
                        && observed["data"]["app_id"] == app
                        && observed["data"]["operation_id"] == operation
                        && observed["data"]["state"] == "Succeeded"
                }
                Err(error) => {
                    report.diagnostic(
                        "fresh URL operation observation",
                        &error.to_string(),
                        "request failed",
                    );
                    false
                }
            }
        } else {
            false
        };
        if settled {
            let remaining = ready_budget().saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            if let Ok(response) = env
                .http
                .get(format!(
                    "{}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/",
                    pingora_base()
                ))
                .timeout(remaining.min(Duration::from_secs(5)))
                .send()
                .await
                && response.status().is_success()
                && response
                    .text()
                    .await
                    .is_ok_and(|text| text.to_ascii_lowercase().contains("<!doctype html"))
            {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(
            Duration::from_secs(3).min(ready_budget().saturating_sub(started.elapsed())),
        )
        .await;
    }
    let result = (|| {
        let current = observe(app, false)?;
        if !ready || current.id == captured.id || current.mounts != captured.mounts {
            return Err(
                "fresh deployment did not create a new ready runtime on the intended mounts"
                    .to_owned(),
            );
        }
        for (source, _) in &current.mounts {
            if source
                .join("e2e-purge-old-marker")
                .try_exists()
                .map_err(|error| error.to_string())?
            {
                return Err("fresh deployment resurrected purged data".into());
            }
        }
        let data = current
            .mounts
            .iter()
            .find(|(_, destination)| destination == shared_types::paths::USERAPP_DEV_DATA)
            .ok_or("data bind missing")?;
        let write = std::process::Command::new("docker")
            .args([
                "exec",
                "--user",
                "0",
                &current.id,
                "python3",
                "-c",
                "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text(sys.argv[2])",
                &format!("{}/e2e-new-data", data.1),
                app,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !write.status.success() {
            return Err("fresh owned runtime data cannot be written".into());
        }
        let marker_path = format!("{}/e2e-new-data", shared_types::paths::USERAPP_DEV_DATA);
        let read = std::process::Command::new("docker")
            .args(["exec", &current.id, "cat", &marker_path])
            .output()
            .map_err(|error| error.to_string())?;
        if !read.status.success() || String::from_utf8_lossy(&read.stdout).trim() != app {
            return Err("fresh runtime data is not readable".into());
        }
        let ids = std::process::Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--no-trunc",
                "--filter",
                &format!("label=rcoder.io/application-id={app}"),
                "--filter",
                "label=service-type=user-app",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !ids.status.success()
            || String::from_utf8_lossy(&ids.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                != [current.id.as_str()]
        {
            return Err("fresh deployment is not a single physical runtime".into());
        }
        Ok(current)
    })();
    report.assert_hard(
        "purge fresh deployment serves HTTP with one runtime and new data",
        result.is_ok(),
        result.as_ref().err().cloned().unwrap_or_default(),
    );
    let diagnostics =
        rcoder_e2e::common::resources::capture_container(&format!("rcoder-app-{app}"));
    report.assert_hard(
        "fresh purge deployment final diagnostics retained",
        diagnostics.is_ok(),
        diagnostics.err().unwrap_or_default(),
    );
    let (status, body) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/delete"),
        json!({"user_id":user,"purge":true}),
    )
    .await;
    report.assert_hard(
        "fresh purge recovery deployment is explicitly cleaned",
        status.is_success() && http_ok(&body),
        format!("{status}: {}", trunc(&body, 200)),
    );
    if let Ok(current) = result {
        let old = old_traffic_stays_deleted(env, app, user, &current).await;
        report.assert_hard(
            "fresh deployment purge remains absent under old traffic",
            old.is_ok(),
            format!("{old:?}"),
        );
    }
}

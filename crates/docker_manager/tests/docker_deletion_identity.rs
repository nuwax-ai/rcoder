//! Real Docker regression; opt in with RCODER_DOCKER_IDENTITY_TEST=1.
//! The explicit test image must provide /bin/sh, sleep, cat, and python3.
//! HOSTNAME must identify the running rcoder container used for network discovery.
use anyhow::{Context, Result, ensure};
use container_runtime_api::{ContainerRuntimeError, UserAppDeploymentRuntime};
use docker_manager::{DockerManager, DockerManagerConfig, runtime::DockerRuntime};
use std::sync::Arc;

async fn docker(args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[tokio::test]
async fn stale_deletion_receipt_preserves_replacement_container_and_volume() {
    if std::env::var("RCODER_DOCKER_IDENTITY_TEST").as_deref() != Ok("1") {
        eprintln!("environment-gated: RCODER_DOCKER_IDENTITY_TEST=1 required");
        return;
    }
    let image = std::env::var("RCODER_DOCKER_TEST_IMAGE").expect("explicit local image required");
    docker(&["image", "inspect", &image])
        .await
        .expect("local image must exist");
    let manager = DockerManager::new(DockerManagerConfig::default())
        .await
        .expect("Docker manager");
    let runtime = DockerRuntime::new(Arc::new(manager));
    let app_id = format!("identity-{}", uuid::Uuid::new_v4().simple());
    let name = format!(
        "{}-{app_id}",
        shared_types::ServiceType::Userapp.container_prefix()
    );
    let volume = format!("rcoder-test-{app_id}");
    let label = format!("{}={app_id}", shared_types::USERAPP_DOCKER_APP_ID_LABEL);
    let mount = format!("{volume}:/proof");
    let run = std::env::var("E2E_RUN_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let case = std::env::var("E2E_CASE_ID").unwrap_or_else(|_| "docker-deletion-identity".into());
    let run_label = format!("rcoder.e2e.run={run}");
    let case_label = format!("rcoder.e2e.case={case}");
    let report_root = std::env::var_os("E2E_REPORT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("rcoder-identity-{run}")));
    let report = report_root.join("docker-lifecycle");
    std::fs::create_dir_all(&report).expect("ownership report directory");
    let ownership_path = report.join("ownership.json");
    let write_ownership = |ids: &[String]| -> Result<()> {
        std::fs::write(
            &ownership_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "run_id":run,"case_id":case,"container_name":name,"volume_name":volume,
                "app_id":app_id,"container_ids":ids
            }))?,
        )?;
        Ok(())
    };
    let mut ids = Vec::new();
    write_ownership(&ids).expect("record ownership before creation");
    docker(&[
        "volume",
        "create",
        "--label",
        "rcoder.test=deletion-identity",
        "--label",
        &run_label,
        "--label",
        &case_label,
        &volume,
    ])
    .await
    .expect("own volume");
    let result: Result<()> = async {
        let old_id = docker(&["run", "-d", "--pull=never", "--network=none", "--name", &name,
            "--label", &label, "--label", &run_label, "--label", &case_label, "--label", "service-type=user-app", "-v", &mount,
            "--entrypoint", "/bin/sh", &image, "-c", "exec sleep 600"]).await?;
        ids.push(old_id.clone());
        write_ownership(&ids)?;
        docker(&["exec", &old_id, "/bin/sh", "-c", "printf old > /proof/marker"]).await?;
        let snapshot = runtime.capture_app_deletion(&app_id, None).await?;
        ensure!(snapshot.resources.len() == 1 && snapshot.resources[0].uid == old_id, "capture physical identity");
        docker(&["rm", "-f", &old_id]).await?;
        let new_id = docker(&["run", "-d", "--pull=never", "--network=none", "--name", &name,
            "--label", &label, "--label", &run_label, "--label", &case_label, "--label", "service-type=user-app", "-v", &mount,
            "--entrypoint", "/bin/sh", &image, "-c", "exec sleep 600"]).await?;
        ids.push(new_id.clone());
        write_ownership(&ids)?;
        docker(&["exec", &new_id, "/bin/sh", "-c", "printf replacement > /proof/marker"]).await?;
        ensure!(matches!(runtime.delete_app_snapshot(&snapshot).await, Err(ContainerRuntimeError::Conflict(_))), "stale receipt must conflict");
        ensure!(docker(&["inspect", "--format", "{{.Id}}", &name]).await? == new_id, "replacement survives");
        ensure!(docker(&["exec", &new_id, "cat", "/proof/marker"]).await? == "replacement", "volume marker survives");
        docker(&["exec", "-d", &new_id, "python3", "-m", "http.server", "9080", "--bind", "127.0.0.1", "--directory", "/proof"]).await?;
        let http_probe = "import time,urllib.request,urllib.error\ndeadline=time.monotonic()+5\nopener=urllib.request.build_opener(urllib.request.ProxyHandler({}))\nwhile True:\n try:\n  print(opener.open('http://127.0.0.1:9080/marker',timeout=1).read().decode());break\n except urllib.error.URLError as error:\n  if not isinstance(error.reason,ConnectionRefusedError) or time.monotonic()>=deadline: raise\n  time.sleep(0.1)";
        ensure!(docker(&["exec", &new_id, "python3", "-c", http_probe]).await? == "replacement", "old application serves HTTP before update");
        let invalid_update = container_runtime_api::ContainerCreateParams::builder()
            .project_id(&app_id).service_type(shared_types::ServiceType::Userapp)
            .image_override("rcoder invalid image reference").build();
        let update_error = runtime.patch_deployment(invalid_update).await.expect_err("invalid image preparation must fail");
        ensure!(matches!(&update_error, ContainerRuntimeError::PreparationFailed(_)), "only confirmed preparation failure permits release");
        ensure!(update_error.to_string().contains("prepare application image"), "failure must occur during image preparation: {update_error}");
        ensure!(docker(&["inspect", "--format", "{{.Id}} {{.State.Running}}", &name]).await? == format!("{new_id} true"), "failed update keeps old running container");
        ensure!(docker(&["exec", &new_id, "cat", "/proof/marker"]).await? == "replacement", "failed update keeps old volume content");
        ensure!(docker(&["exec", &new_id, "python3", "-c", http_probe]).await? == "replacement", "failed update keeps old HTTP response");
        println!("identity_evidence={}", serde_json::json!({"app_id":app_id,"old_container_id":old_id,"new_container_id":new_id,"volume":volume,"image":image,"marker":"replacement"}));
        Ok(())
    }.await;
    let mut cleanup_errors = Vec::new();
    for id in &ids {
        if docker(&["container", "inspect", id]).await.is_ok()
            && let Err(error) = docker(&["rm", "-f", id]).await
        {
            cleanup_errors.push(error.to_string());
        }
    }
    if let Err(error) = docker(&["volume", "rm", &volume]).await {
        cleanup_errors.push(error.to_string());
    }
    assert!(
        cleanup_errors.is_empty(),
        "cleanup failed: {cleanup_errors:?}; result={result:?}"
    );
    result
        .context("real Docker stale deletion regression")
        .expect("identity behavior");
}

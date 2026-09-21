//! deploy-host 宿主机形态生命周期场景（无 LLM 依赖）。
//!
//! 前置：宿主机 rcoder 以 `--features deploy-host` 运行（make dev-host），
//! RCODER_URL 指向其主端口（默认 127.0.0.1）。验证契约：
//! ensure 创建 → 容器端口发布到宿主机（deploy-host 核心差异）→ ensure 幂等
//! 复用 → owned 清理回收容器与端口。
//!
//! 运行: `make test-e2e-host`（严格启动器，五处注册见 tests-e2e/tools/）。

use std::time::Duration;

use rcoder_e2e::common::report::JsonlReporter;
use rcoder_e2e::common::{Env, TestUserGuard};
use serde_json::json;

async fn post_json(
    env: &Env,
    path: &str,
    body: serde_json::Value,
) -> Result<(reqwest::StatusCode, serde_json::Value), String> {
    let url = format!("{}{path}", env.rcoder);
    let resp = env
        .http
        .post(&url)
        .timeout(Duration::from_secs(120))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let value = resp.json().await.unwrap_or(serde_json::Value::Null);
    Ok((status, value))
}

fn docker_inspect_ports(container: &str) -> Result<String, String> {
    let output = std::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{json .NetworkSettings.Ports}}",
            container,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "docker inspect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn docker_container_absent(container: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("name=^/{container}$")])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        _ => false,
    }
}

/// 宿主机形态 gate：launcher 上下文 + /health 可达（compose_or_skip 的宿主机
/// 变体——不要求 LLM 配置；AI 场景另行注册并在缺配置时显式失败）。
pub async fn host_or_skip(scenario: &str) -> Option<(Env, JsonlReporter)> {
    if !rcoder_e2e::common::require_context_or_skip() {
        return None;
    }
    let env = Env::load();
    let report = JsonlReporter::begin(
        scenario,
        "host",
        json!({ "rcoder": env.rcoder, "user": env.user, "trace_id": env.trace_id }),
    );
    let health = env
        .http
        .get(format!("{}/health", env.rcoder))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    match health {
        Ok(r) if r.status().is_success() => Some((env, report)),
        Ok(r) => {
            report.skip(&format!("host gate: /health HTTP {}", r.status()));
            None
        }
        Err(e) => {
            report.skip(&format!("host gate: /health unreachable: {e}"));
            None
        }
    }
}

#[tokio::test]
async fn host_agent_lifecycle_no_llm() {
    let Some((env, report)) = host_or_skip("host_agent_lifecycle_no_llm").await else {
        return;
    };
    let user = env.scoped_user("hostlife");
    let project = format!("{user}-proj");
    let container_name = format!("dev-rcoder-agent-runner-{user}");
    let _guard = TestUserGuard::new(&env, &user);

    // 1) ensure 创建 computer agent 容器（无 LLM 依赖的容器创建路径）
    let (status, body) = post_json(
        &env,
        "/computer/pod/ensure",
        json!({ "user_id": user, "project_id": project }),
    )
    .await
    .expect("ensure request");
    let created_ok = status.is_success() && body["code"].as_str() == Some("0000");
    let container_status = body["data"]["container_info"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    report.assert_hard(
        "host_pod_ensure_created",
        created_ok && container_status == "running",
        format!("HTTP {status}, body={body}, status={container_status}"),
    );

    // 2) deploy-host 核心契约：容器端口发布到宿主机（8086/tcp 有 HostPort 映射）
    let ports_json = docker_inspect_ports(&container_name);
    let published = ports_json
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|ports| {
            ports["8086/tcp"]
                .as_array()
                .and_then(|bindings| bindings.first())
                .and_then(|first| first["HostPort"].as_str())
                .map(|port| !port.is_empty())
        })
        .unwrap_or(false);
    report.assert_hard(
        "host_container_ports_published",
        published,
        format!(
            "ports={}",
            ports_json.as_deref().unwrap_or("<inspect failed>")
        ),
    );

    // 3) ensure 幂等：同身份复用（created=false），不再新建
    let (status2, body2) = post_json(
        &env,
        "/computer/pod/ensure",
        json!({ "user_id": user, "project_id": project }),
    )
    .await
    .expect("ensure idempotent request");
    let reused = status2.is_success()
        && body2["code"].as_str() == Some("0000")
        && body2["data"]["created"].as_bool() == Some(false);
    report.assert_hard(
        "host_ensure_idempotent_reuse",
        reused,
        format!("HTTP {status2}, body={body2}"),
    );

    // 4) owned 清理回收：guard drop 删容器（端口随容器删除释放）
    drop(_guard);
    let mut absent = false;
    for _ in 0..20 {
        if docker_container_absent(&container_name) {
            absent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    report.assert_hard(
        "host_owned_cleanup_reclaims_container",
        absent,
        format!("container={container_name}"),
    );

    assert!(
        report.finish(),
        "host lifecycle hard assertions failed; see report"
    );
}

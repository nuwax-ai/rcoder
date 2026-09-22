//! deploy-host Direct 直拨形态生命周期场景（无 LLM 依赖）。
//!
//! 前置：宿主机 rcoder 以 Direct Reach 运行（`make dev-host-direct`，即
//! RCODER_DEPLOY_HOST_REACH=direct + `--features deploy-host`），RCODER_URL
//! 指向其主端口。验证契约：ensure 创建 → 容器**零端口发布** + 容器 IPv4
//! 宿主机可直拨 → ensure 幂等复用 → owned 清理回收容器。
//!
//! **fail-loud 模式门**：「零发布」是反向断言——若 rcoder 误以 published
//! 模式运行，本套件会假失败。首查 inspect 见任何非空 HostPort 即 hard-fail
//! 并给出前置命令，绝不静默 skip。
//!
//! 运行: `make test-e2e-host-direct`（严格启动器，五处注册见 tests-e2e/tools/）。

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

fn docker_inspect(container: &str, format: &str) -> Result<String, String> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "--format", format, container])
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

/// 容器 IPv4：解析 `{{json .NetworkSettings.Networks}}`（与 ports 断言同款
/// JSON 风格——Go template 直取 `.NetworkSettings.IPAddress` 在 user-defined
/// 网络容器上缺键报错）。优先 deploy-host 主网卡，回退任意非空 IPv4。
fn docker_container_ipv4(container: &str) -> Result<String, String> {
    let networks_json = docker_inspect(container, "{{json .NetworkSettings.Networks}}")?;
    let networks = serde_json::from_str::<serde_json::Value>(&networks_json)
        .map_err(|e| format!("parse networks json failed: {e}"))?;
    let Some(entries) = networks.as_object() else {
        return Ok(String::new());
    };
    let ipv4_of = |value: &serde_json::Value| {
        value["IPAddress"]
            .as_str()
            .filter(|ip| ip.contains('.'))
            .map(str::to_owned)
    };
    let primary = entries
        .get("rcoder-agent-network")
        .and_then(|value| ipv4_of(value).filter(|ip| !ip.is_empty()));
    Ok(primary
        .or_else(|| entries.values().find_map(|value| ipv4_of(value)))
        .unwrap_or_default())
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

/// 宿主机形态 gate（host_docker_lifecycle 同款：launcher 上下文 + /health
/// 可达，不要求 LLM 配置；host_direct 场景名区分报告身份）。
async fn host_direct_or_skip(scenario: &str) -> Option<(Env, JsonlReporter)> {
    if !rcoder_e2e::common::require_context_or_skip() {
        return None;
    }
    let mut env = Env::load();
    env.k8s_ssh.clear();
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
async fn host_agent_direct_lifecycle_no_llm() {
    let Some((env, report)) = host_direct_or_skip("host_agent_direct_lifecycle_no_llm").await
    else {
        return;
    };
    let user = env.scoped_user("hostdirect");
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
        "host_direct_pod_ensure_created",
        created_ok && container_status == "running",
        format!("HTTP {status}, body={body}, status={container_status}"),
    );

    // 2) Direct 核心契约 + fail-loud 模式门：全部端口零 HostPort 发布。
    // 反向断言在 published 栈上会假失败——发现非空 HostPort 时给出前置命令，
    // 不静默 skip。
    let ports_json = docker_inspect(&container_name, "{{json .NetworkSettings.Ports}}");
    let any_published = ports_json
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .map(|ports| {
            ports
                .as_object()
                .map(|entries| {
                    entries.values().any(|bindings| {
                        bindings.as_array().is_some_and(|list| {
                            list.iter().any(|binding| {
                                binding["HostPort"]
                                    .as_str()
                                    .is_some_and(|port| !port.is_empty())
                            })
                        })
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);
    report.assert_hard(
        "host_direct_no_published_ports",
        !any_published,
        if any_published {
            format!(
                "rcoder 未运行 direct 模式（容器 {container_name} 存在非空 HostPort 发布）；\
                 前置：make dev-host-direct 后重跑。ports={}",
                ports_json.as_deref().unwrap_or("<inspect failed>")
            )
        } else {
            format!(
                "ports={}",
                ports_json.as_deref().unwrap_or("<inspect failed>")
            )
        },
    );

    // 3) 容器 IPv4 宿主机直拨：Direct 模式拨号地址 = {ip}:{容器端口原值}，
    //    实拨 agent HTTP 8086 /health 断 2xx（5s 超时）
    let ipv4 = docker_container_ipv4(&container_name);
    let dial_ok = match ipv4.as_deref() {
        Ok(ip) if !ip.is_empty() => {
            let url = format!("http://{ip}:8086/health");
            match env
                .http
                .get(&url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) => {
                    let ok = resp.status().is_success();
                    report.diagnostic("direct_dial", &format!("HTTP {}", resp.status()), &url);
                    ok
                }
                Err(e) => {
                    report.diagnostic("direct_dial", "connect failed", &e.to_string());
                    false
                }
            }
        }
        other => {
            report.diagnostic("direct_dial", "ipv4 unavailable", &format!("{other:?}"));
            false
        }
    };
    report.assert_hard(
        "host_direct_container_ip_dial",
        dial_ok,
        format!("ipv4={:?}", ipv4.as_deref().unwrap_or("<unavailable>")),
    );

    // 4) ensure 幂等：同身份复用（created=false），不再新建
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
        "host_direct_ensure_idempotent_reuse",
        reused,
        format!("HTTP {status2}, body={body2}"),
    );

    // 5) owned 清理回收：guard drop 删容器
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
        "host_direct_owned_cleanup_reclaims_container",
        absent,
        format!("container={container_name}"),
    );

    assert!(
        report.finish(),
        "host direct lifecycle hard assertions failed; see report"
    );
}

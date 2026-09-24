//! deploy-host 宿主机 **K8s 形态**生命周期场景（无 LLM 依赖）。
//!
//! 前置：宿主机 rcoder 以 `--features kubernetes,deploy-host` + 本地 kubeconfig
//! （OrbStack k3s 等）运行，且满足三前置——userApp 控制面 PG
//! （RCODER_USERAPP_PG_URL）、kubernetes_config.services 的 resource_limits、
//! 共享 computer workspace PVC（`{ns}-rcoder-computer-workspace`，单节点
//! local-path/RWO 即可）。误在 Docker 形态上跑会因 kubectl 查不到
//! Service 而失败（严格验收语义）。
//!
//! 验证契约：ensure 建 STS → Service NodePort 化且端口已分配 →
//! **NodePort 宿主可达**（注册表行为端到端证据）→ ensure 幂等复用 →
//! owned 清理回收 K8s 资源。

use std::time::Duration;

use rcoder_e2e::common::Env;
use rcoder_e2e::common::report::JsonlReporter;
use serde_json::Value;
use serde_json::json;

const AGENT_PREFIX: &str = "dev-rcoder-agent-runner";

async fn post_json(
    env: &Env,
    path: &str,
    body: Value,
) -> Result<(reqwest::StatusCode, Value), String> {
    let url = format!("{}{path}", env.rcoder);
    let resp = env
        .http
        .post(&url)
        .timeout(Duration::from_secs(180))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, value))
}

fn kubectl(args: &[&str]) -> Result<String, String> {
    let namespace = local_test_namespace()?;
    let output = std::process::Command::new("kubectl")
        .args(["--namespace", &namespace])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "kubectl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Local K8s tests only operate in the namespace explicitly shared with the
/// host RCoder process. A default kube context or `.env.local` fallback is not
/// sufficient evidence of ownership.
fn local_test_namespace() -> Result<String, String> {
    let namespace = std::env::var("TEST_K8S_NS").map_err(|_| "TEST_K8S_NS is required")?;
    let runtime_namespace =
        std::env::var("RCODER_K8S_NAMESPACE").map_err(|_| "RCODER_K8S_NAMESPACE is required")?;
    if !namespace.starts_with("rcoder-") || namespace != runtime_namespace {
        return Err(
            "local K8s E2E requires a dedicated rcoder-* namespace matching RCODER_K8S_NAMESPACE"
                .into(),
        );
    }
    if std::env::var_os("KUBECONFIG").is_none() {
        return Err("an explicit KUBECONFIG is required for local K8s E2E".into());
    }
    Ok(namespace)
}

fn kubectl_json(args: &[&str]) -> Result<Value, String> {
    serde_json::from_str(&kubectl(args)?).map_err(|error| format!("kubectl JSON: {error}"))
}

fn selected(kind: &str, app_id: &str) -> Result<Vec<Value>, String> {
    let label = format!("rcoder.io/identifier={app_id},rcoder.io/service-type=user-app-builder");
    let list = kubectl_json(&["get", kind, "-l", &label, "-o", "json"])?;
    list["items"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("{kind} list has no items"))
}

fn only_one(kind: &str, app_id: &str) -> Result<Value, String> {
    let mut items = selected(kind, app_id)?;
    if items.len() != 1 {
        return Err(format!(
            "expected one {kind} for {app_id}, found {}",
            items.len()
        ));
    }
    Ok(items.remove(0))
}

struct K8sUserAppProbe {
    pod_uid: Option<String>,
    pvc_name: Option<String>,
    pvc_uid: Option<String>,
}

impl K8sUserAppProbe {
    fn new() -> Self {
        Self {
            pod_uid: None,
            pvc_name: None,
            pvc_uid: None,
        }
    }
    fn pvc_identity(&self) -> Result<bool, String> {
        let name = self
            .pvc_name
            .as_deref()
            .ok_or("PVC name was not captured")?;
        let pvc = kubectl_json(&["get", "pvc", name, "-o", "json"])?;
        Ok(pvc["metadata"]["uid"].as_str() == self.pvc_uid.as_deref())
    }
}

#[async_trait::async_trait]
impl rcoder_e2e::common::userapp_compute::DevComputeProbe for K8sUserAppProbe {
    async fn prepare(&mut self, app_id: &str, marker: &str) -> Result<(), String> {
        let _sts = only_one("sts", app_id)?;
        let pod = only_one("pod", app_id)?;
        let pod_name = pod["metadata"]["name"].as_str().ok_or("Pod name missing")?;
        self.pod_uid = pod["metadata"]["uid"].as_str().map(str::to_owned);
        let pvc_name = pod["spec"]["volumes"]
            .as_array()
            .and_then(|volumes| volumes.iter().find(|volume| volume["name"] == "workspace"))
            .and_then(|volume| volume["persistentVolumeClaim"]["claimName"].as_str())
            .ok_or("builder workspace PVC claim missing")?
            .to_owned();
        let pvc = kubectl_json(&["get", "pvc", &pvc_name, "-o", "json"])?;
        self.pvc_uid = pvc["metadata"]["uid"].as_str().map(str::to_owned);
        self.pvc_name = Some(pvc_name);
        if self.pod_uid.is_none() || self.pvc_uid.is_none() {
            return Err("Pod or PVC UID missing".into());
        }
        let path = format!("/home/user/{app_id}/.e2e-compute-marker");
        kubectl(&[
            "exec",
            pod_name,
            "-c",
            "agent",
            "--",
            "sh",
            "-c",
            "printf '%s' \"$1\" > \"$2\"",
            "e2e",
            marker,
            &path,
        ])?;
        Ok(())
    }

    async fn stopped(&mut self, app_id: &str) -> Result<(), String> {
        if !selected("pod", app_id)?.is_empty() {
            return Err("builder Pod still exists after completed Stop".into());
        }
        let sts = only_one("sts", app_id)?;
        if sts["spec"]["replicas"].as_i64() != Some(0) {
            return Err("builder StatefulSet was not scaled to zero".into());
        }
        if !self.pvc_identity()? {
            return Err("workspace PVC changed during Stop".into());
        }
        Ok(())
    }

    async fn restarted(&mut self, app_id: &str, marker: &str) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        loop {
            let pods = selected("pod", app_id)?;
            if pods.len() == 1 && pods[0]["status"]["phase"] == "Running" {
                let pod = &pods[0];
                let uid = pod["metadata"]["uid"]
                    .as_str()
                    .ok_or("new Pod UID missing")?;
                if Some(uid) == self.pod_uid.as_deref() {
                    return Err("Restart reused the stopped Pod UID".into());
                }
                if !self.pvc_identity()? {
                    return Err("workspace PVC changed during Restart".into());
                }
                let name = pod["metadata"]["name"]
                    .as_str()
                    .ok_or("new Pod name missing")?;
                let path = format!("/home/user/{app_id}/.e2e-compute-marker");
                if let Ok(observed) = kubectl(&["exec", name, "-c", "agent", "--", "cat", &path]) {
                    if observed != marker {
                        return Err(format!(
                            "workspace marker changed after Restart: {observed:?}"
                        ));
                    }
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("restarted builder did not become usable".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// K8s 资源 guard：Drop 时删该 identifier 的 STS 与 Service（panic/断言失败
/// 路径同样回收——对齐 Docker 场景 TestUserGuard 的 Drop 语义，避免失败轮
/// 遗留 sts/svc 阻塞下一轮同名创建）。共享 PVC 永不删除。
struct K8sResourceGuard {
    identifier: String,
}

impl Drop for K8sResourceGuard {
    fn drop(&mut self) {
        k8s_cleanup(&self.identifier);
    }
}

/// 场景兜底清理：删该 identifier 的 STS 与两个 Service（label 选择器对齐
/// rcoder.io/identifier）。幂等；agent PVC 属共享 PVC，永不删除。
fn k8s_cleanup(identifier: &str) {
    let target =
        format!("rcoder.io/identifier={identifier},rcoder.io/service-type=computer-agent-runner");
    for kind in ["sts", "svc"] {
        match kubectl(&["delete", kind, "-l", &target, "--ignore-not-found=true"]) {
            Ok(out) if !out.is_empty() => {
                eprintln!("  [cleanup] {kind} {identifier}: {out}");
            }
            _ => {}
        }
    }
    eprintln!("  [cleanup] {identifier}: k8s sts+svc deleted");
}

#[tokio::test]
async fn host_k8s_agent_lifecycle_no_llm() {
    let scenario = "host_k8s_agent_lifecycle_no_llm";
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let mut env = Env::load();
    // host 场景绝不继承远端清理目标（本地 kubectl 直连）
    env.k8s_ssh.clear();
    let report = JsonlReporter::begin(
        scenario,
        "host",
        json!({ "rcoder": env.rcoder, "user": env.user, "trace_id": env.trace_id }),
    );
    let namespace = local_test_namespace();
    report.assert_hard(
        "host K8s explicit namespace",
        namespace.is_ok(),
        format!("{namespace:?}"),
    );
    if namespace.is_err() {
        assert!(report.finish(), "host K8s namespace precondition failed");
        return;
    }
    let health = env
        .http
        .get(format!("{}/health", env.rcoder))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    match health {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            report.skip(&format!("host-k8s gate: /health HTTP {}", r.status()));
            return;
        }
        Err(e) => {
            report.skip(&format!("host-k8s gate: /health unreachable: {e}"));
            return;
        }
    }
    // 本地 kubectl 可用性（形态误配时后续 Service 断言会失败并归因）
    if kubectl(&["get", "svc", "-o", "name"]).is_err() {
        report.skip("host-k8s gate: local kubectl unavailable");
        return;
    }

    let user = env.scoped_user("hostk8s");
    let identifier = user.clone();
    let sts_name = format!("{AGENT_PREFIX}-{identifier}");
    let svc_name = format!("{sts_name}-svc");
    let _guard = K8sResourceGuard {
        identifier: identifier.clone(),
    };

    // 1) ensure 创建 computer agent STS（K8s 形态）
    let (status, body) = post_json(
        &env,
        "/computer/pod/ensure",
        json!({ "user_id": user, "project_id": user }),
    )
    .await
    .expect("ensure request");
    let created_ok = status.is_success() && body["code"].as_str() == Some("0000");
    let container_status = body["data"]["container_info"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    report.assert_hard(
        "host_k8s_pod_ensure_created",
        created_ok && container_status == "running",
        format!("HTTP {status}, status={container_status}, body_head={}", {
            let text = body.to_string();
            text.chars().take(160).collect::<String>()
        }),
    );

    // 2) Service NodePort 化且 8086 端口 nodePort 已分配（apiserver 异步）
    let mut node_port = String::new();
    for _ in 0..20 {
        if let Ok(np) = kubectl(&[
            "get",
            "svc",
            &svc_name,
            "-o",
            "jsonpath={.spec.ports[?(@.port==8086)].nodePort}",
        ]) && !np.is_empty()
        {
            node_port = np;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let svc_type =
        kubectl(&["get", "svc", &svc_name, "-o", "jsonpath={.spec.type}"]).unwrap_or_default();
    report.assert_hard(
        "host_k8s_service_nodeport_assigned",
        svc_type == "NodePort" && !node_port.is_empty(),
        format!("type={svc_type}, nodePort(8086)={node_port}"),
    );

    // 3) NodePort 宿主可达（deploy-host 注册表行为端到端证据）。
    // Pod Running ≠ endpoints ready（readiness 未过时 NodePort 无后端会拒连），
    // 有界轮询直至 endpoints 就绪 + 探针通过
    let mut nodeport_reachable = false;
    let mut probe_desc = String::from("not probed");
    let node_ip = std::env::var("RCODER_K8S_NODE_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    for _ in 0..30 {
        let probe = env
            .http
            .get(format!("http://{node_ip}:{node_port}/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        match &probe {
            Ok(r) if r.status().is_success() => {
                nodeport_reachable = true;
                probe_desc = format!("HTTP {}", r.status());
                break;
            }
            Ok(r) => probe_desc = format!("HTTP {}", r.status()),
            Err(e) => probe_desc = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    report.assert_hard(
        "host_k8s_nodeport_reachable",
        nodeport_reachable,
        format!("probe=127.0.0.1:{node_port}/health -> {probe_desc}"),
    );

    // 4) ensure 幂等：同身份复用（created=false）
    let (status2, body2) = post_json(
        &env,
        "/computer/pod/ensure",
        json!({ "user_id": user, "project_id": user }),
    )
    .await
    .expect("ensure idempotent request");
    let reused = status2.is_success()
        && body2["code"].as_str() == Some("0000")
        && body2["data"]["created"].as_bool() == Some(false);
    report.assert_hard(
        "host_k8s_ensure_idempotent_reuse",
        reused,
        format!("HTTP {status2}, body_head={}", {
            let text = body2.to_string();
            text.chars().take(140).collect::<String>()
        }),
    );

    // 5) owned 清理：guard Drop 回收 sts+svc（共享 PVC 永不删），此处轮询确认
    drop(_guard);
    let mut gone = false;
    for _ in 0..30 {
        let sts = kubectl(&[
            "get",
            "sts",
            &sts_name,
            "--ignore-not-found=true",
            "-o",
            "name",
        ])
        .unwrap_or_default();
        let svc = kubectl(&[
            "get",
            "svc",
            &svc_name,
            "--ignore-not-found=true",
            "-o",
            "name",
        ])
        .unwrap_or_default();
        if sts.is_empty() && svc.is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    report.assert_hard(
        "host_k8s_owned_cleanup_reclaims",
        gone,
        format!("sts={sts_name} svc={svc_name}"),
    );

    assert!(
        report.finish(),
        "host k8s lifecycle hard assertions failed; see report"
    );
}

/// The shared UserApp compute contract on a local K8s runtime. The test owns a
/// unique app ID and purges it through RCoder after verifying Stop retained the
/// PVC and Restart mounted the same UID. A failed purge is reported, never
/// replaced by an unverified direct kubectl delete.
#[tokio::test]
async fn host_k8s_userapp_dev_compute_no_llm() {
    use rcoder_e2e::common::userapp_compute;
    let scenario = "host_k8s_userapp_dev_compute_no_llm";
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let mut env = Env::load();
    env.k8s_ssh.clear();
    let report = JsonlReporter::begin(
        scenario,
        "host",
        json!({"rcoder": env.rcoder, "trace_id": env.trace_id}),
    );
    let namespace = local_test_namespace();
    report.assert_hard(
        "host K8s explicit namespace",
        namespace.is_ok(),
        format!("{namespace:?}"),
    );
    if namespace.is_err() {
        assert!(report.finish(), "host K8s namespace precondition failed");
        return;
    }
    let health = env
        .http
        .get(format!("{}/health", env.rcoder))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    let healthy = health
        .as_ref()
        .is_ok_and(|response| response.status().is_success());
    report.assert_hard("host K8s RCoder reachable", healthy, format!("{health:?}"));
    if !healthy {
        assert!(report.finish(), "host K8s RCoder unavailable");
        return;
    }
    let case_id = std::env::var("E2E_CASE_ID").expect("strict case identity");
    let app = format!("e2e{}", &case_id[..12]);
    let mut probe = K8sUserAppProbe::new();
    let lifecycle =
        userapp_compute::run_dev_compute_cycle(&env, &report, &app, &mut probe, false).await;
    let purge = userapp_compute::purge_owned_app(&env, &app, lifecycle.as_deref()).await;
    report.assert_hard(
        "userapp owned resources purged",
        purge.is_ok(),
        format!("{purge:?}"),
    );
    if purge.is_ok() {
        let remaining_pods = selected("pod", &app);
        let remaining_sts = selected("sts", &app);
        let remaining_pvc = probe
            .pvc_name
            .as_deref()
            .map(|name| kubectl(&["get", "pvc", name, "--ignore-not-found=true", "-o", "name"]));
        let gone = remaining_pods.as_ref().is_ok_and(Vec::is_empty)
            && remaining_sts.as_ref().is_ok_and(Vec::is_empty)
            && remaining_pvc
                .as_ref()
                .is_none_or(|result| result.as_ref().is_ok_and(String::is_empty));
        report.assert_hard(
            "userapp K8s test resources absent after purge",
            gone,
            format!("pods={remaining_pods:?}, sts={remaining_sts:?}, pvc={remaining_pvc:?}"),
        );
    }
    assert!(report.finish(), "host K8s UserApp compute contract failed");
}

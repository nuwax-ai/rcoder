//! 测试公共层：环境加载与门控、chat 请求、SSE 场景通用编排件。
//!
//! 与 Python 套件（tests/sse_e2e/common.py）语义对齐；配置读取同为
//! "环境变量优先，仓库根 .env.local 兜底"（API key 等不进 git）。

pub mod metrics;
pub mod report;
pub mod scenario;
pub mod sse;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use report::{ChatTrace, JsonlReporter};
use serde_json::{Value, json};
use shared_types::ChatAgentConfig;
use shared_types::ChatAgentServerConfig;
use shared_types::ChatResponse;
use shared_types::ComputerChatRequest;
use shared_types::ModelProviderConfig;

/// agent 后端选择：openai=nuwaxcode(opencode) | anthropic=claude-code-acp-ts。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Openai,
    Anthropic,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Openai => "openai",
            Backend::Anthropic => "anthropic",
        }
    }
}

pub struct Env {
    pub rcoder: String,
    pub api_key: String,
    /// openai 协议模型代理 base_url
    pub base_url: String,
    /// anthropic 协议模型代理 base_url
    pub base_url_anthropic: String,
    pub model: String,
    /// 切模型场景的目标模型（LLM_MODEL_PRO；空 = 未配置，切模型场景 skip）
    pub model_pro: String,
    /// 本进程 run 标签（报告目录/请求 id 前缀）
    pub run_tag: String,
    /// 场景独立 user 前缀（ue + HHMMSS，K8s 资源名 ≤63 → user ≤23 字符约束）
    pub user: String,
    /// K8s 模式（TEST_K8S_SSH）：清理走远程 kubectl；空 = Docker 模式。
    /// 所有 IP/主机均走配置（.env.local 或环境变量），代码零硬编码。
    pub k8s_ssh: String,
    pub k8s_ns: String,
    /// K8s 节点入口列表（LB_ENTRY_HOSTS，逗号分隔；单入口 = 场景退化同入口）
    pub lb_entry_hosts: String,
    /// K8s NodePort 端口（LB_NODEPORT，默认 30295）
    pub lb_nodeport: String,
    /// 本场景的 W3C trace id（注入 traceparent header；jsonl 记录供检索）
    pub trace_id: String,
    pub http: reqwest::Client,
    /// SSE 专用 client（与 chat 隔离连接池：排除 keep-alive 连接复用变量）
    pub sse_http: reqwest::Client,
}

/// 仓库根 .env.local（KEY=VALUE 行；gitignore，含 API key）。
/// 与 Python 套件同位（tests/sse_e2e/common.py：HERE.parent.parent/.env.local）。
fn load_env_local() -> HashMap<String, String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".env.local");
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut cfg = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            cfg.insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    cfg
}

fn cfg_or_env(cfg: &HashMap<String, String>, key: &str) -> String {
    cfg_or_env_or(cfg, key, "")
}

/// 环境变量 > .env.local > default（对齐 Python 套件读取优先级）。
fn cfg_or_env_or(cfg: &HashMap<String, String>, key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get(key).cloned().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| default.to_owned())
}

impl Env {
    pub fn load() -> Self {
        let cfg = load_env_local();
        let run_tag = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let hhmmss = run_tag.split('_').nth(1).unwrap_or("000000").to_owned();
        // W3C traceparent 注入：e2e 发起 trace（每 Env 实例 = 每场景独立
        // trace_id），rcoder 侧 make_span_with_trace_parent 提取继承——
        // OTLP 开启时全链路同一 trace，失败排查用 trace_id 检索。
        let trace_id = uuid::Uuid::new_v4().simple().to_string();
        let span_id = &uuid::Uuid::new_v4().simple().to_string()[..16];
        let traceparent = format!("00-{trace_id}-{span_id}-01");
        let mut default_headers = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&traceparent) {
            default_headers.insert("traceparent", v);
        }
        Env {
            rcoder: cfg_or_env_or(&cfg, "RCODER_URL", "http://127.0.0.1:8090"),
            api_key: cfg_or_env(&cfg, "LLM_API_KEY"),
            base_url: cfg_or_env(&cfg, "LLM_BASE_URL"),
            base_url_anthropic: cfg
                .get("LLM_BASE_URL_ANTHROPIC")
                .cloned()
                .unwrap_or_default(),
            model: cfg_or_env(&cfg, "LLM_MODEL"),
            model_pro: cfg_or_env(&cfg, "LLM_MODEL_PRO"),
            run_tag: run_tag.clone(),
            user: format!("ue{hhmmss}"),
            k8s_ssh: cfg_or_env(&cfg, "TEST_K8S_SSH"),
            k8s_ns: cfg_or_env_or(&cfg, "TEST_K8S_NS", "nuwax-k8s-test"),
            lb_entry_hosts: cfg_or_env(&cfg, "LB_ENTRY_HOSTS"),
            lb_nodeport: cfg_or_env_or(&cfg, "LB_NODEPORT", "30295"),
            trace_id,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .default_headers(default_headers.clone())
                .build()
                .expect("build http client"),
            sse_http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .default_headers(default_headers)
                .build()
                .expect("build sse http client"),
        }
    }

    /// 每场景独立 user → 独立 agent 容器/进程（同 user 的 agent 是 prompt 串行的，
    /// 前一场景未完结的 turn 会被下一场景 chat cancel）。
    pub fn scoped_user(&self, name: &str) -> String {
        format!("{}-{name}", self.user)
    }

    /// compose 门控：探测 /health（2s）+ LLM 配置完整性；任一不满足 → skip。
    /// `cargo test --workspace` 在无环境机器上由此保持全绿（PG-gated 同模式）。
    pub async fn compose_or_skip(scenario: &str, backend: &str) -> Option<(Self, JsonlReporter)> {
        let env = Self::load();
        let report = JsonlReporter::begin(
            scenario,
            backend,
            json!({ "rcoder": env.rcoder, "model": env.model, "user": env.user, "trace_id": env.trace_id }),
        );
        if env.api_key.is_empty() || env.model.is_empty() || env.base_url.is_empty() {
            report.skip(
                "LLM config missing: 检查 .env.local / LLM_API_KEY / LLM_MODEL / LLM_BASE_URL \
                 （空 model_provider 会让 agent 调用失败、场景慢死）",
            );
            return None;
        }
        let health = env
            .http
            .get(format!("{}/health", env.rcoder))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        match health {
            Ok(r) if r.status().is_success() => Some((env, report)),
            Ok(r) => {
                report.skip(&format!("compose gate: /health HTTP {}", r.status()));
                None
            }
            Err(e) => {
                report.skip(&format!("compose gate: /health unreachable: {e}"));
                None
            }
        }
    }

    /// 构造 chat 请求（对齐 Python base_payload；backend 决定 agent 与协议）。
    pub fn base_payload(
        &self,
        backend: Backend,
        prompt: &str,
        request_id: &str,
        user: &str,
    ) -> ComputerChatRequest {
        self.base_payload_with_model(backend, prompt, request_id, user, "")
    }

    /// 模型覆盖版（model_override 非空时替换 provider 的 id/name/default_model，
    /// 对齐 Python base_payload 的 model= 参数——切模型场景用）。
    pub fn base_payload_with_model(
        &self,
        backend: Backend,
        prompt: &str,
        request_id: &str,
        user: &str,
        model_override: &str,
    ) -> ComputerChatRequest {
        let m = if model_override.is_empty() {
            self.model.clone()
        } else {
            model_override.to_owned()
        };
        let (provider, server) = match backend {
            Backend::Anthropic => {
                let provider = ModelProviderConfig {
                    id: m.clone(),
                    name: m.clone(),
                    base_url: self.base_url_anthropic.clone(),
                    api_key: self.api_key.clone(),
                    default_model: m.clone(),
                    requires_openai_auth: true,
                    api_protocol: Some("anthropic".to_owned()),
                    wire_api: None,
                };
                let server = ChatAgentServerConfig {
                    agent_id: Some("claude-code-acp-ts".to_owned()),
                    command: Some("claude-code-acp-ts".to_owned()),
                    args: Some(Vec::new()),
                    env: Some(HashMap::from([
                        (
                            "ANTHROPIC_API_KEY".to_owned(),
                            "{MODEL_PROVIDER_API_KEY}".to_owned(),
                        ),
                        (
                            "ANTHROPIC_MODEL".to_owned(),
                            "{MODEL_PROVIDER_DEFAULT_MODEL}".to_owned(),
                        ),
                        (
                            "ANTHROPIC_BASE_URL".to_owned(),
                            "{MODEL_PROVIDER_BASE_URL}".to_owned(),
                        ),
                    ])),
                    ..Default::default()
                };
                (provider, server)
            }
            Backend::Openai => {
                let provider = ModelProviderConfig {
                    id: m.clone(),
                    name: m.clone(),
                    base_url: self.base_url.clone(),
                    api_key: self.api_key.clone(),
                    default_model: m.clone(),
                    requires_openai_auth: true,
                    api_protocol: Some("openai".to_owned()),
                    wire_api: None,
                };
                let server = ChatAgentServerConfig {
                    agent_id: Some("nuwaxcode".to_owned()),
                    command: Some("nuwaxcode".to_owned()),
                    args: Some(vec!["acp".to_owned()]),
                    env: Some(HashMap::from([
                        (
                            "OPENAI_API_KEY".to_owned(),
                            "{MODEL_PROVIDER_API_KEY}".to_owned(),
                        ),
                        (
                            "OPENCODE_MODEL".to_owned(),
                            "openai-compatible/{MODEL_PROVIDER_DEFAULT_MODEL}".to_owned(),
                        ),
                        (
                            "OPENAI_BASE_URL".to_owned(),
                            "{MODEL_PROVIDER_BASE_URL}".to_owned(),
                        ),
                    ])),
                    ..Default::default()
                };
                (provider, server)
            }
        };
        ComputerChatRequest {
            user_id: user.to_owned(),
            project_id: None,
            service_type: None,
            prompt: prompt.to_owned(),
            session_id: None,
            attachments: Vec::new(),
            data_source_attachments: Vec::new(),
            model_provider: Some(provider),
            request_id: Some(request_id.to_owned()),
            system_prompt: Some("你是集成测试助手。严格按要求输出，不要解释。".to_owned()),
            user_prompt: None,
            agent_config: Some(ChatAgentConfig {
                agent_server: Some(server),
                ..Default::default()
            }),
            pod_id: None,
            tenant_id: None,
            space_id: None,
            isolation_type: None,
            agent_work_dir: None,
        }
    }
}

/// 向指定入口发 chat 并校验（HttpResult 包装；success 字段 serde skip，
/// 判定用 code == "0000"）。低层版（client+url），供 spawned 后台任务复用。
pub async fn chat_via(
    client: &reqwest::Client,
    url: &str,
    req: &ComputerChatRequest,
) -> Result<ChatResponse> {
    let resp = client
        .post(format!("{url}/computer/chat"))
        .timeout(Duration::from_secs(180))
        .json(req)
        .send()
        .await
        .map_err(|e| anyhow!("chat request: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("parse chat response: {e}"))?;
    if !status.is_success() || body["code"].as_str() != Some("0000") {
        bail!("chat failed via {url}: HTTP {status}, body: {body}");
    }
    let data: ChatResponse = serde_json::from_value(body["data"].clone())
        .map_err(|e| anyhow!("chat response data 反序列化: {e}"))?;
    Ok(data)
}

/// Env 版（默认入口）。
pub async fn chat_at(env: &Env, url: &str, req: &ComputerChatRequest) -> Result<ChatResponse> {
    chat_via(&env.http, url, req).await
}

/// chat 全流程留痕版：失败也写入 jsonl 后返回 Err。
pub async fn chat_reported(
    env: &Env,
    report: &JsonlReporter,
    phase: &str,
    url: &str,
    req: &ComputerChatRequest,
) -> Result<ChatResponse> {
    let t0 = Instant::now();
    let req_sanitized = sanitize_request(req);
    let result = chat_at(env, url, req).await;
    let (ok, resp_value, error) = match &result {
        Ok(data) => (
            true,
            Some(serde_json::to_value(data).unwrap_or_default()),
            None,
        ),
        Err(e) => (false, None, Some(e.to_string())),
    };
    report.chat_request(ChatTrace {
        phase,
        url,
        ok,
        request_sanitized: req_sanitized,
        response: resp_value.as_ref(),
        error: error.as_deref(),
        elapsed_ms: t0.elapsed().as_millis(),
    });
    result
}

/// 请求脱敏（api_key 永不落盘；保留 id/default_model/base_url 供排查）。
pub fn sanitize_request(req: &ComputerChatRequest) -> Value {
    let mut v = serde_json::to_value(req).unwrap_or_default();
    if let Some(mp) = v["modelProvider"].as_object_mut() {
        mp.remove("apiKey");
    } else if let Some(mp) = v["model_provider"].as_object_mut() {
        mp.remove("api_key");
    }
    v
}

/// 场景清理 guard：Drop 时删除该 user 的 agent 容器（不等闲置回收）。
/// Docker 模式 docker rm；K8s 模式（TEST_K8S_SSH）远程 kubectl 删
/// STS/svc/PVC（ns 硬限定 + user 前缀严格匹配双重保护，与 Python 套件一致）。
pub struct TestUserGuard {
    pub user: String,
    k8s: Option<(String, String)>,
}

impl TestUserGuard {
    pub fn new(env: &Env, user: &str) -> Self {
        TestUserGuard {
            user: user.to_owned(),
            k8s: (!env.k8s_ssh.is_empty()).then(|| (env.k8s_ssh.clone(), env.k8s_ns.clone())),
        }
    }
}

impl Drop for TestUserGuard {
    fn drop(&mut self) {
        let outcome = match &self.k8s {
            Some((ssh, ns)) => cleanup_k8s(ssh, ns, &self.user),
            None => cleanup_docker(&self.user),
        };
        eprintln!("  [cleanup] {}: {outcome}", self.user);
    }
}

fn cleanup_docker(user: &str) -> String {
    let name_filter = format!("dev-rcoder-agent-runner-{user}");
    let out = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("name={name_filter}")])
        .output();
    let Ok(out) = out else {
        return "docker ps failed".to_owned();
    };
    let ids = String::from_utf8_lossy(&out.stdout);
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return "no containers".to_owned();
    }
    let mut args = vec!["rm", "-f"];
    args.extend(ids.iter().copied());
    match std::process::Command::new("docker").args(&args).output() {
        Ok(o) if o.status.success() => format!("removed {} container(s)", ids.len()),
        Ok(o) => format!("rm failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("docker rm exec failed: {e}"),
    }
}

fn cleanup_k8s(ssh: &str, ns: &str, user: &str) -> String {
    let list = std::process::Command::new("ssh")
        .args([ssh, "kubectl", "-n", ns, "get", "sts,svc,pvc", "-o", "name"])
        .output();
    let Ok(out) = list else {
        return "ssh kubectl get failed".to_owned();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = format!("rcoder-computer-agent-runner-{user}");
    let targets: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/'))
        .filter(|l| l.rsplit('/').next().is_some_and(|n| n.starts_with(&prefix)))
        .map(str::to_owned)
        .collect();
    if targets.is_empty() {
        return "no resources".to_owned();
    }
    let mut args = vec![
        "kubectl".to_owned(),
        "-n".to_owned(),
        ns.to_owned(),
        "delete".to_owned(),
    ];
    args.extend(targets.iter().cloned());
    match std::process::Command::new("ssh")
        .args([ssh])
        .args(&args)
        .output()
    {
        Ok(o) if o.status.success() => format!("deleted {} resource(s)", targets.len()),
        Ok(o) => format!("delete failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("ssh kubectl delete exec failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_unique_cases() {
        assert!(sse::monotonic_unique(&[]));
        assert!(sse::monotonic_unique(&[1, 2, 3]));
        assert!(!sse::monotonic_unique(&[1, 2, 2]));
        assert!(!sse::monotonic_unique(&[3, 2]));
    }

    #[test]
    fn snippet_detects_verbatim_replay() {
        // 逐字重放窗口 win=24：文本须长于窗口才会被检测（Python 原版同语义）
        let a = "雨刚停，街面上浮着一层薄薄的水光，像一面镜子一样，倒映着灰白的天空";
        assert!(sse::longest_common_snippet(a, a, 24).is_some());
        // 语义复述不命中
        assert!(sse::longest_common_snippet(a, "雨停了，地面湿了反光", 24).is_none());
        // 短于窗口的文本不检测
        let short = "太短了";
        assert!(sse::longest_common_snippet(short, short, 24).is_none());
    }

    #[test]
    fn sanitize_removes_api_key() {
        let mut env = Env::load();
        env.api_key = "sk-SECRET".to_owned();
        let req = env.base_payload(Backend::Openai, "hi", "r1", "u1");
        let s = sanitize_request(&req);
        assert!(!s.to_string().contains("sk-SECRET"));
    }
}

/// 跨进程文件锁（flock 独占，持有到 guard drop）。
///
/// compose_userapp 与 compose_userapp_dev 是两个测试二进制（cargo test 并行跑），
/// 各自的进程内 OnceLock 串行锁互相不可见——都建 rcoder-app-builder-* 容器，
/// 并发时单节点资源竞争复现"后发容器 agent_runner 启动超退避窗"。本锁以
/// 共享文件 flock 实现跨二进制互斥。
/// 跨进程互斥锁（TCP 端口占位：bind 成功即持锁，进程退出由内核自动释放，
/// 无锁文件残留问题）。compose_userapp 与 compose_userapp_dev 是两个测试二进制
/// （cargo test 并行跑），各自的进程内 OnceLock 串行锁互相不可见——都建
/// rcoder-app-builder-* 容器，并发时单节点资源竞争复现"后发容器 agent_runner
/// 启动超退避窗"。经独占端口互斥，实现跨二进制串行。
pub mod cross_bin_lock {
    use std::net::TcpListener;
    use std::sync::OnceLock;

    /// 锁端口：高位非常见服务端口，专属本测试框架。
    const LOCK_PORT: u16 = 39471;

    static HELD: OnceLock<TcpListener> = OnceLock::new();

    /// 阻塞获取跨二进制互斥锁（另一测试二进制持有时自旋等待；
    /// 其进程退出后端口立即释放）。
    pub fn acquire() {
        if HELD.get().is_some() {
            return;
        }
        loop {
            match TcpListener::bind(("127.0.0.1", LOCK_PORT)) {
                Ok(listener) => {
                    // set 失败=本进程二次 acquire（HELD.get 短路已挡）；listener 存
                    // 入 static 持有到进程退出——端口即锁，内核在进程退出时释放。
                    if HELD.set(listener).is_err() {
                        unreachable!("cross_bin_lock acquired twice in one process");
                    }
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }
}

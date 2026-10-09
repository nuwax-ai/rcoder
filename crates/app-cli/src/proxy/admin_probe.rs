//! Pingap admin 只读探测通道：仅用于确认配置重载实际生效（config_hash 比对）。
//!
//! ## 只读纪律（不可违背）
//! 本模块**永不使用 admin 写端点**（POST /api/configs、POST /api/restart 等）；
//! TOML + `--autoreload` 始终是配置的唯一权威来源，admin 只是 app-cli 启动 pingap 时
//! 经 env 注入的 loopback 观察点，用来读取 `GET /api/basic` 返回的 `config_hash`。
//!
//! ## 鉴权算法（pingap src/plugin/admin.rs:272-330，非 Basic Auth）
//! 请求头 `Authorization: {token}:{ts}`，其中 token = sha256_hex("{user}:{pass}:{ts}")
//! 小写十六进制，ts 为 unix 秒，须落在 pingap 的 max_age 窗口内。
//!
//! ## hash 语义（pingap-config PingapConfig::hash()）
//! descriptions(category/name/data) 拼接后 CRC32（大写 hex）。app-cli pin 同一 rev 的
//! pingap-config，对内存中的 `PingapConfig` 直接调 `.hash()` 即得期望值；pingap 加载
//! 同一 TOML 走 `PingapConfig::new(bytes, true)` 得到相同 descriptions → 相同 hash。

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// admin 默认监听端口（仅 loopback），可用 `APP_CLI_PINGAP_ADMIN_PORT` 覆盖。
pub const DEFAULT_ADMIN_PORT: u16 = 3018;
const ADMIN_PORT_ENV: &str = "APP_CLI_PINGAP_ADMIN_PORT";

/// 生效确认轮询间隔与默认总预算（autoreload tick ≤10s + 文件监听即时热更，25s 足够）。
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
pub const CONFIRM_BUDGET: Duration = Duration::from_secs(25);
/// 回切后对旧 hash 的二次确认预算（best-effort，失败仅 warn）。
pub const ROLLBACK_CONFIRM_BUDGET: Duration = Duration::from_secs(10);

/// admin 端点凭证。每次进程启动随机生成；密码不落盘、不进日志。
#[derive(Clone)]
pub struct AdminEndpoint {
    pub addr: String,
    pub user: String,
    pub password: String,
}

static ADMIN_ENDPOINT: OnceLock<AdminEndpoint> = OnceLock::new();

/// Explicit invalid configuration is never silently replaced by another port.
pub fn admin_port() -> Result<u16> {
    match std::env::var(ADMIN_PORT_ENV) {
        Ok(value) => {
            let port = value.parse::<u16>().context("invalid proxy admin port")?;
            anyhow::ensure!(port != 0, "proxy admin port must not be zero");
            Ok(port)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_ADMIN_PORT),
        Err(error) => Err(error).context("read proxy admin port"),
    }
}

/// 注册 admin 端点；后续 reload 确认读取。重复注册以首次为准。
pub fn register_admin_endpoint(
    addr: String,
    user: String,
    password: String,
) -> &'static AdminEndpoint {
    ADMIN_ENDPOINT.get_or_init(|| AdminEndpoint {
        addr,
        user,
        password,
    })
}

/// 进程级 ensure：优先从常驻 spec（P1 常驻代理）继承凭证——跨 owner 进程
/// 存活的 pingap 仍持有首任凭证，新 owner 必须用同一凭证才能探 admin
///（standby 确认/hash 确认）；无常驻 spec 时生成随机凭证并注册（后续调用
/// 复用，凭证生命周期绑定 app-cli 进程——supervisord 托管下 pingap program
/// 崩溃重启由 supervisord 用同一 spec/凭证拉起，probe 侧无需刷新）。
pub fn ensure_admin_endpoint() -> Result<&'static AdminEndpoint> {
    if let Some(endpoint) = ADMIN_ENDPOINT.get() {
        validate_endpoint(endpoint)?;
        return Ok(endpoint);
    }
    let endpoint = match endpoint_from_resident_spec()? {
        Some(endpoint) => {
            tracing::info!(
                addr = %endpoint.addr,
                "admin endpoint inherited from resident pingap spec"
            );
            endpoint
        }
        None => {
            let user = uuid::Uuid::new_v4().simple().to_string();
            let password = uuid::Uuid::new_v4().simple().to_string();
            AdminEndpoint {
                addr: format!("127.0.0.1:{}", admin_port()?),
                user,
                password,
            }
        }
    };
    validate_endpoint(&endpoint)?;
    Ok(ADMIN_ENDPOINT.get_or_init(|| endpoint))
}

/// 常驻 spec（`resident/pingap.toml`，0600）中的 admin 凭证恢复。
/// 解析失败/文件缺失返回 None（生成新凭证——后续首个常驻编排会覆写 spec）。
fn endpoint_from_resident_spec() -> Result<Option<AdminEndpoint>> {
    let path = crate::svc_spec::spec_root()
        .join(crate::svc_spec::RESIDENT_SPEC_ID)
        .join("pingap.toml");
    match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("observe resident proxy credential record"),
        Ok(metadata) => anyhow::ensure!(metadata.is_file(), "resident proxy record is not a file"),
    }
    let spec = crate::svc_spec::ServiceSpecFile::load(crate::svc_spec::RESIDENT_SPEC_ID, "pingap")?;
    verify_proxy_spec(&spec)?;
    endpoint_from_spec_env(&spec.env).map(Some)
}

/// 凭证候选目录（C3 迁移：resident 优先；缺失时任意代际的
/// `*/pingap.toml` 同含 admin 凭证——同容器旧版升级后存活的 pingap 仍持
/// 旧代凭证，新 owner 必须恢复同一组）。字典序确定遍历。
fn verify_proxy_spec(spec: &crate::svc_spec::ServiceSpecFile) -> Result<()> {
    anyhow::ensure!(
        spec.service_id == "pingap" && spec.port.is_none(),
        "not a platform proxy credential record"
    );
    let binary = spec.argv.first().context("proxy spec executable missing")?;
    anyhow::ensure!(
        std::path::Path::new(binary)
            .file_name()
            .is_some_and(|name| name == "pingap" || name == "pingap.exe"),
        "credential record belongs to a business command"
    );
    anyhow::ensure!(
        spec.argv.len() == 4
            && spec.argv[1] == "-c"
            && spec.argv[3] == "--autoreload"
            && std::path::Path::new(&spec.argv[2]).is_absolute(),
        "proxy specification command does not match its launch contract"
    );
    Ok(())
}

fn endpoint_from_spec_env(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<AdminEndpoint> {
    let addr = env
        .get("PINGAP_ADMIN_ADDR")
        .context("proxy credential address missing")?;
    let user = env
        .get("PINGAP_ADMIN_USER")
        .context("proxy credential user missing")?;
    let password = env
        .get("PINGAP_ADMIN_PASSWORD")
        .context("proxy credential password missing")?;
    let endpoint = AdminEndpoint {
        addr: addr.clone(),
        user: user.clone(),
        password: password.clone(),
    };
    validate_endpoint(&endpoint)?;
    Ok(endpoint)
}

fn validate_endpoint(endpoint: &AdminEndpoint) -> Result<()> {
    let address: std::net::SocketAddr = endpoint
        .addr
        .parse()
        .context("invalid proxy admin socket address")?;
    anyhow::ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "proxy admin must bind a loopback socket"
    );
    anyhow::ensure!(
        !endpoint.user.is_empty() && !endpoint.password.is_empty(),
        "proxy admin requires nonempty credentials"
    );
    Ok(())
}

/// Called only after the host has bound the legacy program PID/start/argv to
/// this exact spec. Directory order and business service names grant no trust.
pub(crate) fn register_verified_legacy_endpoint(
    spec: &crate::svc_spec::ServiceSpecFile,
) -> Result<&'static AdminEndpoint> {
    verify_proxy_spec(spec)?;
    let endpoint = endpoint_from_spec_env(&spec.env)?;
    if let Some(existing) = ADMIN_ENDPOINT.get() {
        anyhow::ensure!(
            existing.addr == endpoint.addr
                && existing.user == endpoint.user
                && existing.password == endpoint.password,
            "proxy credentials changed during owner lifetime"
        );
        return Ok(existing);
    }
    Ok(ADMIN_ENDPOINT.get_or_init(|| endpoint))
}

pub fn admin_endpoint() -> Option<&'static AdminEndpoint> {
    ADMIN_ENDPOINT.get()
}

/// 构造 admin 鉴权头：`{sha256_hex(user:pass:ts)}:{ts}`（小写 hex）。
pub fn authorization_header(user: &str, password: &str, ts: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{user}:{password}:{ts}").as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{hex}:{ts}")
}

fn now_unix_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .context("system clock is before unix epoch")
}

/// 从 `GET /api/basic` 响应体解析 `config_hash`。
pub fn parse_config_hash(body: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("parse admin /api/basic JSON body")?;
    value
        .get("config_hash")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("admin /api/basic response missing config_hash"))
}

/// 单个 upstream 的健康视图（pingap `upstream_healthy_status` 值形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamHealthView {
    pub healthy: u32,
    pub total: u32,
}

/// admin `/api/basic` 的只读快照（业务就绪观察消费的字段子集）。
#[derive(Debug, Clone)]
pub struct AdminBasicSnapshot {
    /// 当前实际生效配置 hash（缺失 = 观察不完整）
    pub config_hash: Option<String>,
    /// upstream 名（= service_id）→ 健康视图
    pub upstreams: std::collections::HashMap<String, UpstreamHealthView>,
}

/// 解析 `/api/basic` 为 [`AdminBasicSnapshot`]（未知字段忽略，向前兼容）。
pub fn parse_admin_basic(body: &str) -> Result<AdminBasicSnapshot> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("parse admin /api/basic JSON body")?;
    let config_hash = value
        .get("config_hash")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let mut upstreams = std::collections::HashMap::new();
    if let Some(map) = value
        .get("upstream_healthy_status")
        .and_then(serde_json::Value::as_object)
    {
        for (name, status) in map {
            let healthy = status
                .get("healthy")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            let total = status
                .get("total")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            upstreams.insert(name.clone(), UpstreamHealthView { healthy, total });
        }
    }
    Ok(AdminBasicSnapshot {
        config_hash,
        upstreams,
    })
}

/// 单次读取 admin `/api/basic` 快照（短超时 connect 1s / total 3s；只读端点）。
pub async fn fetch_admin_basic(endpoint: &AdminEndpoint) -> Result<AdminBasicSnapshot> {
    let client = build_probe_client()?;
    let ts = now_unix_seconds()?;
    let url = format!("http://{}/api/basic", endpoint.addr);
    let response = client
        .get(&url)
        .header(
            "Authorization",
            authorization_header(&endpoint.user, &endpoint.password, ts),
        )
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("read admin response body from {url}"))?;
    if !status.is_success() {
        anyhow::bail!("admin probe {url} returned {status}");
    }
    parse_admin_basic(&body)
}

/// hash 比对：pingap 输出大写 hex，本地 `.hash()` 同格式；仍做大小写无关比对防御格式漂移。
pub fn hashes_match(expected: &str, actual: &str) -> bool {
    !expected.is_empty() && expected.eq_ignore_ascii_case(actual)
}

/// 单次探测：读 pingap 当前生效配置的 config_hash（短超时 connect 1s / total 3s）。
pub async fn fetch_config_hash(addr: &str, user: &str, password: &str) -> Result<String> {
    let client = build_probe_client()?;
    fetch_with_client(&client, addr, user, password).await
}

pub async fn fetch_apply_status(
    endpoint: &AdminEndpoint,
) -> Result<super::apply_status::ApplyStatus> {
    validate_endpoint(endpoint)?;
    let client = build_probe_client()?;
    fetch_application(&client, endpoint).await
}

async fn fetch_application(
    client: &reqwest::Client,
    endpoint: &AdminEndpoint,
) -> Result<super::apply_status::ApplyStatus> {
    let url = format!("http://{}/api/apply-status", endpoint.addr);
    let response = client
        .get(&url)
        .header(
            "Authorization",
            authorization_header(&endpoint.user, &endpoint.password, now_unix_seconds()?),
        )
        .send()
        .await
        .context("read proxy application result")?;
    anyhow::ensure!(
        response.status().is_success(),
        "proxy does not provide an authenticated application result (HTTP {})",
        response.status()
    );
    let bytes = response
        .bytes()
        .await
        .context("read proxy application result body")?;
    serde_json::from_slice(&bytes).context("decode proxy application result")
}

/// One total deadline covers the application result and every listener probe.
/// The hash is only a consistency field; an Applied graph, UUID and process
/// identity must match before the data plane can confirm this operation.
pub async fn wait_for_publication(
    endpoint: &AdminEndpoint,
    expected: &super::compiler::CompileOutcome,
    budget: Duration,
) -> Result<super::apply_status::ConfirmedPublication> {
    validate_endpoint(endpoint)?;
    let deadline = tokio::time::Instant::now() + budget;
    let client = build_probe_client()?;
    let mut instance: Option<(u32, String)> = None;
    let mut last = "publication has not been observed".to_string();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "proxy publication {} was not confirmed: {last}",
                expected.publication_id
            );
        }
        let observed =
            match tokio::time::timeout_at(deadline, fetch_application(&client, endpoint)).await {
                Ok(Ok(observed)) => observed,
                Ok(Err(error)) => {
                    last = format!("{error:#}");
                    tokio::time::sleep(PROBE_INTERVAL.min(remaining)).await;
                    continue;
                }
                Err(_) => anyhow::bail!(
                    "proxy publication observation exceeded its total deadline: {last}"
                ),
            };
        let current = (observed.process_id, observed.process_instance_id.clone());
        if let Some(ref first) = instance {
            anyhow::ensure!(
                *first == current,
                "proxy instance changed while confirming publication"
            );
        } else {
            instance = Some(current);
        }
        if let Some(confirmed) = observed.confirmed(expected)? {
            for target in &expected.entry_probes {
                let url = format!("http://{}/_pub/{}", target.address, expected.publication_id);
                let response = tokio::time::timeout_at(deadline, client.get(&url).send())
                    .await
                    .context("proxy listener verification deadline exhausted")?
                    .with_context(|| format!("verify proxy listener {}", target.address))?;
                anyhow::ensure!(
                    response.status().as_u16() == target.expected_status,
                    "proxy publication listener status mismatch"
                );
                anyhow::ensure!(
                    response
                        .headers()
                        .get("X-Rcoder-Publication")
                        .and_then(|value| value.to_str().ok())
                        == Some(&expected.publication_id),
                    "proxy listener returned another publication"
                );
                let bytes = tokio::time::timeout_at(deadline, response.bytes())
                    .await
                    .context("proxy publication body deadline exhausted")??;
                anyhow::ensure!(
                    bytes.as_ref() == expected.publication_id.as_bytes(),
                    "proxy listener did not execute this publication marker"
                );
            }
            // A marker can execute while a declared backend is unavailable.
            // Serving confirmation requires the release's real HTTP contract.
            for target in &expected.business_probes {
                let url = format!("http://{}{}", target.address, target.path);
                let response = tokio::time::timeout_at(deadline, client.get(&url).send())
                    .await
                    .context("business HTTP verification deadline exhausted")?
                    .with_context(|| format!("verify business service {}", target.service_id))?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "business service {} failed its HTTP contract (HTTP {})",
                    target.service_id,
                    response.status()
                );
            }
            // A listener marker cannot authorize a response from another process
            // that replaced the authenticated admin during the HTTP observations.
            let after = tokio::time::timeout_at(deadline, fetch_application(&client, endpoint))
                .await
                .context("proxy final application observation deadline exhausted")??;
            anyhow::ensure!(
                after.process_id == confirmed.process_id
                    && after.process_instance_id == confirmed.instance_id,
                "proxy instance changed after listener verification"
            );
            anyhow::ensure!(
                after.confirmed(expected)?.is_some(),
                "proxy application changed after listener verification"
            );
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "proxy confirmation completed after its deadline"
            );
            return Ok(confirmed);
        }
        last = "current Applied graph belongs to another publication".into();
        tokio::time::sleep(
            PROBE_INTERVAL.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

fn build_probe_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Credentials and local observations must never use OS/environment
        // proxies. Discovering a system proxy also consumes the local deadline.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("build admin probe HTTP client")
}

async fn fetch_with_client(
    client: &reqwest::Client,
    addr: &str,
    user: &str,
    password: &str,
) -> Result<String> {
    let ts = now_unix_seconds()?;
    let url = format!("http://{addr}/api/basic");
    let response = client
        .get(&url)
        .header("Authorization", authorization_header(user, password, ts))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("read admin response body from {url}"))?;
    if !status.is_success() {
        anyhow::bail!("admin probe {url} returned {status}");
    }
    parse_config_hash(&body)
}

/// 轮询确认 pingap 生效配置的 config_hash 与期望一致。
///
/// 间隔 1s、总预算 `budget`；admin 连不上（如 pingap 刚重启）会在预算内重试，
/// 最终仍失败则返回错误（绝不静默跳过确认）。返回 Ok 表示已确认生效。
pub async fn wait_for_config_hash(
    endpoint: &AdminEndpoint,
    expected_hash: &str,
    budget: Duration,
) -> Result<()> {
    let client = build_probe_client()?;
    let deadline = tokio::time::Instant::now() + budget;
    let mut last_observed: Option<String> = None;
    let mut last_error: Option<anyhow::Error> = None;
    loop {
        match fetch_with_client(&client, &endpoint.addr, &endpoint.user, &endpoint.password).await {
            Ok(hash) if hashes_match(expected_hash, &hash) => return Ok(()),
            Ok(hash) => {
                last_observed = Some(hash);
                last_error.take();
            }
            Err(error) => last_error = Some(error),
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
    if let Some(error) = last_error {
        Err(error).with_context(|| {
            format!(
                "pingap admin probe unreachable/misbehaving after {}s; cannot confirm config took effect",
                budget.as_secs()
            )
        })
    } else {
        anyhow::bail!(
            "config_hash mismatch after {}s: expected {expected_hash}, observed {}",
            budget.as_secs(),
            last_observed.as_deref().unwrap_or("<none>")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1：常驻 spec 存在时优先恢复其凭证（跨 owner 存活的 pingap 仍持有
    /// 首任凭证；新 owner 生成新凭证将无法探 admin）。spec 路径受
    /// APP_CLI_SPEC_DIR 控制，与其它 spec 测试同锁串行。
    #[test]
    fn endpoint_recovery_prefers_resident_spec_credentials() {
        let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("APP_CLI_SPEC_DIR", dir.path()) };
        // 无常驻 spec：恢复 None（走生成路径）
        assert!(endpoint_from_resident_spec().unwrap().is_none());

        let spec = crate::svc_spec::ServiceSpecFile {
            release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
            service_id: "pingap".into(),
            cwd: "/".into(),
            argv: vec![
                "/usr/local/bin/pingap".into(),
                "-c".into(),
                "/app/logs/pingap/active/pingap.toml".into(),
                "--autoreload".into(),
            ],
            env: [
                (
                    "PINGAP_ADMIN_ADDR".to_string(),
                    "127.0.0.1:19086".to_string(),
                ),
                ("PINGAP_ADMIN_USER".to_string(), "u-prev".to_string()),
                ("PINGAP_ADMIN_PASSWORD".to_string(), "p-prev".to_string()),
            ]
            .into_iter()
            .collect(),
            port: None,
        };
        spec.write().unwrap();
        let endpoint = endpoint_from_resident_spec()
            .unwrap()
            .expect("recover from resident spec");
        assert_eq!(endpoint.addr, "127.0.0.1:19086");
        assert_eq!(endpoint.user, "u-prev");
        assert_eq!(endpoint.password, "p-prev");
    }

    use super::{authorization_header, hashes_match, parse_config_hash};

    #[test]
    fn authorization_header_matches_pingap_sha256_scheme() {
        // 固定向量：sha256("admin:secret:1700000000") 预计算值
        // （与 pingap plugin/admin.rs auth_validate 的 sha256(user:pass:ts) 一致）。
        let header = authorization_header("admin", "secret", 1_700_000_000);
        assert_eq!(
            header,
            "c91900e874b42905e1ae19f86c778afdd6ca787ea1c0e1990b8a046a2a668a95:1700000000"
        );
        // 格式：{64 位小写 hex}:{ts}
        let (token, ts) = header.split_once(':').expect("header has token:ts shape");
        assert_eq!(token.len(), 64);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(ts, "1700000000");
    }

    #[test]
    fn authorization_header_changes_with_credentials_and_time() {
        let a = authorization_header("admin", "secret", 1_700_000_000);
        let b = authorization_header("admin", "secret", 1_700_000_001);
        let c = authorization_header("admin", "other", 1_700_000_000);
        assert_ne!(a, b, "timestamp participates in the hash");
        assert_ne!(a, c, "password participates in the hash");
    }

    #[test]
    fn parse_config_hash_extracts_field() {
        let body = r#"{"version":"0.13.8","config_hash":"AB12CD34","pid":"1"}"#;
        assert_eq!(
            parse_config_hash(body).expect("config_hash present"),
            "AB12CD34"
        );
    }

    #[test]
    fn parse_config_hash_rejects_missing_or_invalid_body() {
        assert!(parse_config_hash(r#"{"pid":"1"}"#).is_err());
        assert!(parse_config_hash(r#"{"config_hash":123}"#).is_err());
        assert!(parse_config_hash("not json").is_err());
    }

    #[test]
    fn hash_comparison_is_case_insensitive_and_rejects_empty() {
        assert!(hashes_match("AB12CD34", "ab12cd34"));
        assert!(hashes_match("AB12CD34", "AB12CD34"));
        assert!(!hashes_match("AB12CD34", "DEADBEEF"));
        assert!(!hashes_match("", ""), "empty expected must never match");
    }

    #[test]
    fn parse_admin_basic_extracts_hash_and_upstreams() {
        let body = r#"{
            "version":"0.14.3",
            "config_hash":"AB12CD34",
            "upstream_healthy_status":{
                "frontend":{"healthy":1,"total":1,"unhealthy_backends":[]},
                "backend":{"healthy":0,"total":1,"unhealthy_backends":["127.0.0.1:4101"]}
            }
        }"#;
        let snapshot = super::parse_admin_basic(body).expect("valid body");
        assert_eq!(snapshot.config_hash.as_deref(), Some("AB12CD34"));
        assert_eq!(
            snapshot
                .upstreams
                .get("frontend")
                .map(|u| (u.healthy, u.total)),
            Some((1, 1))
        );
        assert_eq!(
            snapshot
                .upstreams
                .get("backend")
                .map(|u| (u.healthy, u.total)),
            Some((0, 1))
        );
    }

    #[test]
    fn parse_admin_basic_tolerates_missing_sections() {
        let snapshot = super::parse_admin_basic(r#"{"version":"0.14.3"}"#).expect("valid body");
        assert_eq!(snapshot.config_hash, None);
        assert!(snapshot.upstreams.is_empty());
        // 非法 JSON 仍是错误（协议错误不能当观察成功）
        assert!(super::parse_admin_basic("not json").is_err());
    }
}

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

/// 解析 admin 端口：env `APP_CLI_PINGAP_ADMIN_PORT` 覆盖，非法值退回默认。
pub fn admin_port() -> u16 {
    match std::env::var(ADMIN_PORT_ENV) {
        Ok(value) => value.parse().unwrap_or_else(|_| {
            tracing::warn!(
                "⚠️  invalid {ADMIN_PORT_ENV}={value}, falling back to {DEFAULT_ADMIN_PORT}"
            );
            DEFAULT_ADMIN_PORT
        }),
        Err(_) => DEFAULT_ADMIN_PORT,
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
pub fn ensure_admin_endpoint() -> &'static AdminEndpoint {
    ADMIN_ENDPOINT.get_or_init(|| {
        if let Some(endpoint) = endpoint_from_resident_spec() {
            tracing::info!(
                addr = %endpoint.addr,
                "admin endpoint inherited from resident pingap spec"
            );
            return endpoint;
        }
        let user = uuid::Uuid::new_v4().simple().to_string();
        let password = uuid::Uuid::new_v4().simple().to_string();
        AdminEndpoint {
            addr: format!("127.0.0.1:{}", admin_port()),
            user,
            password,
        }
    })
}

/// 常驻 spec（`resident/pingap.toml`，0600）中的 admin 凭证恢复。
/// 解析失败/文件缺失返回 None（生成新凭证——后续首个常驻编排会覆写 spec）。
fn endpoint_from_resident_spec() -> Option<AdminEndpoint> {
    for release_id in spec_candidate_directories() {
        let Ok(spec) = crate::svc_spec::ServiceSpecFile::load(&release_id, "pingap") else {
            continue;
        };
        if let Some(endpoint) = endpoint_from_spec_env(&spec.env) {
            return Some(endpoint);
        }
    }
    None
}

/// 凭证候选目录（C3 迁移：resident 优先；缺失时任意代际的
/// `*/pingap.toml` 同含 admin 凭证——同容器旧版升级后存活的 pingap 仍持
/// 旧代凭证，新 owner 必须恢复同一组）。字典序确定遍历。
fn spec_candidate_directories() -> Vec<String> {
    let root = crate::svc_spec::spec_root();
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    entry
                        .file_type()
                        .ok()
                        .filter(|kind| kind.is_dir())
                        .map(|_| entry.file_name().to_string_lossy().into_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.retain(|name| name != crate::svc_spec::RESIDENT_SPEC_ID);
    names.insert(0, crate::svc_spec::RESIDENT_SPEC_ID.to_string());
    names
}

fn endpoint_from_spec_env(
    env: &std::collections::BTreeMap<String, String>,
) -> Option<AdminEndpoint> {
    let addr = env.get("PINGAP_ADMIN_ADDR")?;
    let user = env.get("PINGAP_ADMIN_USER")?;
    let password = env.get("PINGAP_ADMIN_PASSWORD")?;
    Some(AdminEndpoint {
        addr: addr.clone(),
        user: user.clone(),
        password: password.clone(),
    })
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

fn build_probe_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
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
        assert!(endpoint_from_resident_spec().is_none());

        let spec = crate::svc_spec::ServiceSpecFile {
            release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
            service_id: "pingap".into(),
            cwd: "/".into(),
            argv: vec!["pingap".into()],
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
        let endpoint = endpoint_from_resident_spec().expect("recover from resident spec");
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

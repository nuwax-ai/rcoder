use super::*;

// ── PG 等待 ──────────────────────────────────────────────────────────────────

/// PostgreSQL preflight is an execution-environment policy supplied by RCoder.
/// Standalone app-cli does not infer a local database from migrations or templates.
pub(crate) fn workspace_needs_pg(_specs: &[ServiceSpec]) -> bool {
    pg_required_by_policy(std::env::var_os("APP_CLI_REQUIRE_PG").as_deref())
}

/// [`workspace_needs_pg`] 的纯谓词（供测试直测，不动进程 env）：
/// 平台声明（builder 形态注入 APP_CLI_REQUIRE_PG=1）才探测；其余值/缺失
/// 均不探测——含声明了 migrate 的服务（e39591126 起 PG 预检为环境策略，
/// 不再由服务清单推断）。
pub(crate) fn pg_required_by_policy(declared: Option<&std::ffi::OsStr>) -> bool {
    declared == Some(std::ffi::OsStr::new("1"))
}

/// Probe the same database and credentials that migration commands consume.
/// A listening PostgreSQL server can still be creating POSTGRES_DB asynchronously;
/// pg_isready cannot establish database existence or successful authentication.
pub(crate) async fn wait_for_pg(
    specs: &[ServiceSpec],
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    if std::env::var_os("APP_CLI_SKIP_PG_WAIT").is_some() {
        warn!("APP_CLI_SKIP_PG_WAIT set; skipping PostgreSQL readiness check (dev only)");
        return Ok(());
    }
    let mut targets = Vec::new();
    let forced = std::env::var_os("APP_CLI_REQUIRE_PG").is_some_and(|value| value == "1");
    for spec in specs
        .iter()
        .filter(|spec| spec.enabled && (forced || !spec.run.migrate.is_empty()))
    {
        targets.push(pg_probe_environment(service_environment(spec, pg)?)?);
    }
    // Explicit APP_CLI_REQUIRE_PG without migrations uses the same runtime defaults.
    if targets.is_empty() {
        let mut environment = std::collections::BTreeMap::new();
        if let Some(pg) = pg {
            environment.insert("POSTGRES_USER".into(), pg.username.clone());
            environment.insert("POSTGRES_PASSWORD".into(), pg.password.clone());
            if let Ok(url) = std::env::var("DATABASE_URL") {
                environment.insert(
                    "DATABASE_URL".into(),
                    database_url_with_credentials(&url, pg)?,
                );
            }
        }
        targets.push(pg_probe_environment(environment)?);
    }
    wait_for_pg_targets(
        "psql",
        &targets,
        Duration::from_secs(60),
        Duration::from_secs(2),
        cancel,
    )
    .await
}

pub(super) fn pg_probe_environment(
    overrides: std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let value = |name: &str, default: &str| -> Result<String> {
        if let Some(value) = overrides.get(name) {
            return Ok(if value.is_empty() {
                default.into()
            } else {
                value.clone()
            });
        }
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(default.into()),
            Err(_) => anyhow::bail!("PostgreSQL runtime environment must be valid Unicode"),
        }
    };
    let url = value("DATABASE_URL", "")?;
    let mut target = std::collections::BTreeMap::new();
    // Match child command inheritance plus per-service overrides for libpq TLS
    // and session settings; URI fields below have higher precedence.
    for name in [
        "PGSSLMODE",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGSSLROOTCERT",
        "PGSSLCRL",
        "PGSSLCRLDIR",
        "PGSSLSNI",
        "PGSSLMINPROTOCOLVERSION",
        "PGSSLMAXPROTOCOLVERSION",
        "PGCHANNELBINDING",
        "PGGSSENCMODE",
        "PGKRBSRVNAME",
        "PGGSSLIB",
        "PGTARGETSESSIONATTRS",
        "PGOPTIONS",
        "PGAPPNAME",
        "PGCLIENTENCODING",
    ] {
        let configured = value(name, "")?;
        if !configured.is_empty() {
            target.insert(name.into(), configured);
        }
    }
    if !url.is_empty() {
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| anyhow::anyhow!("Runtime DATABASE_URL is invalid"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "postgres" | "postgresql"),
            "Runtime DATABASE_URL must use PostgreSQL"
        );
        // PGDATABASE is a literal database name, not libpq's expandable `dbname`
        // argument. Decode the URI into libpq environment settings so credentials
        // never enter argv. Reject unsupported options instead of probing another
        // target silently.
        if let Some(host) = parsed.host_str() {
            target.insert(
                "PGHOST".into(),
                host.trim_start_matches('[').trim_end_matches(']').into(),
            );
        }
        if let Some(port) = parsed.port() {
            target.insert("PGPORT".into(), port.to_string());
        }
        if !parsed.username().is_empty() {
            target.insert("PGUSER".into(), decode_pg_uri_component(parsed.username())?);
        }
        if let Some(password) = parsed.password() {
            target.insert("PGPASSWORD".into(), decode_pg_uri_component(password)?);
        }
        if let Some(database) = parsed
            .path()
            .strip_prefix('/')
            .filter(|value| !value.is_empty())
        {
            target.insert("PGDATABASE".into(), decode_pg_uri_component(database)?);
        }
        if let Some(query) = parsed.query() {
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (name, value) = pair
                    .split_once('=')
                    .context("Invalid PostgreSQL URI option")?;
                let name = decode_pg_uri_component(name)?;
                let value = decode_pg_uri_component(value)?;
                let variable = match name.as_str() {
                    "host" => "PGHOST",
                    "hostaddr" => "PGHOSTADDR",
                    "port" => "PGPORT",
                    "user" => "PGUSER",
                    "password" => "PGPASSWORD",
                    "dbname" => "PGDATABASE",
                    "sslmode" => "PGSSLMODE",
                    "sslcert" => "PGSSLCERT",
                    "sslkey" => "PGSSLKEY",
                    "sslrootcert" => "PGSSLROOTCERT",
                    "sslcrl" => "PGSSLCRL",
                    "sslcrldir" => "PGSSLCRLDIR",
                    "sslsni" => "PGSSLSNI",
                    "ssl_min_protocol_version" => "PGSSLMINPROTOCOLVERSION",
                    "ssl_max_protocol_version" => "PGSSLMAXPROTOCOLVERSION",
                    "channel_binding" => "PGCHANNELBINDING",
                    "gssencmode" => "PGGSSENCMODE",
                    "krbsrvname" => "PGKRBSRVNAME",
                    "gsslib" => "PGGSSLIB",
                    "target_session_attrs" => "PGTARGETSESSIONATTRS",
                    "options" => "PGOPTIONS",
                    "application_name" => "PGAPPNAME",
                    "client_encoding" => "PGCLIENTENCODING",
                    "connect_timeout" => "PGCONNECT_TIMEOUT",
                    // Older PostgreSQL URI clients accept ssl=true as require.
                    "ssl" if value == "true" => {
                        target.insert("PGSSLMODE".into(), "require".into());
                        continue;
                    }
                    _ => anyhow::bail!("Unsupported PostgreSQL URI option for startup probe"),
                };
                target.insert(variable.into(), value);
            }
        }
    } else {
        target.insert("PGHOST".into(), value("PGHOST", "localhost")?);
        target.insert("PGPORT".into(), value("PGPORT", "5432")?);
        target.insert("PGUSER".into(), value("POSTGRES_USER", "dev")?);
        target.insert("PGPASSWORD".into(), value("POSTGRES_PASSWORD", "dev")?);
        target.insert("PGDATABASE".into(), value("POSTGRES_DB", "dev")?);
    }
    Ok(target)
}

pub(super) fn decode_pg_uri_component(value: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes
                .next()
                .and_then(|value| char::from(value).to_digit(16));
            let low = bytes
                .next()
                .and_then(|value| char::from(value).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                anyhow::bail!("Invalid PostgreSQL URI percent encoding");
            };
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    anyhow::ensure!(!decoded.contains(&0), "PostgreSQL URI contains a null byte");
    String::from_utf8(decoded).map_err(|_| anyhow::anyhow!("PostgreSQL URI must be valid Unicode"))
}

pub(super) async fn wait_for_pg_targets(
    program: &str,
    targets: &[std::collections::BTreeMap<String, String>],
    budget: Duration,
    interval: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let never_cancelled = tokio_util::sync::CancellationToken::new();
    let cancel = cancel.unwrap_or(&never_cancelled);
    for target in targets {
        loop {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "PostgreSQL readiness wait cancelled"
            );
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "PostgreSQL target database is not accessible within the startup budget"
            );
            let mut command = Command::new(program);
            command
                .args([
                    "-X",
                    "--no-password",
                    "-qAt",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-c",
                    "SELECT 1",
                ])
                .env_remove("PGHOSTADDR")
                .env_remove("PGSERVICE")
                .envs(target)
                .env("PGCONNECT_TIMEOUT", "2")
                .env(
                    "PGOPTIONS",
                    format!(
                        "{} -c statement_timeout=2000",
                        target.get("PGOPTIONS").map(String::as_str).unwrap_or("")
                    ),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Both engines may be dropped by their owning control future.
                // psql is invoked directly (no shell or background descendants).
                .kill_on_drop(true);
            let mut child = spawn_owned(command, None)
                .await
                .context("spawn PostgreSQL target database probe")?;
            if let Some(mut pipe) = child.take_stdout() {
                tokio::spawn(async move {
                    drop(tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await);
                });
            }
            if let Some(mut pipe) = child.take_stderr() {
                tokio::spawn(async move {
                    drop(tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await);
                });
            }
            let attempt_deadline =
                deadline.min(tokio::time::Instant::now() + Duration::from_secs(3));
            let outcome = tokio::select! {
                _ = cancel.cancelled() => None,
                result = tokio::time::timeout_at(attempt_deadline, child.wait_root()) => Some(result),
            };
            if child.stop(Duration::ZERO).await == StopOutcome::Unconfirmed {
                process_utils::command_context::retain_cleanup(Some(child), None);
                anyhow::bail!("PostgreSQL probe cleanup remains unconfirmed");
            }
            match outcome {
                Some(Ok(Ok(status))) if status.success() => {
                    anyhow::ensure!(
                        !cancel.is_cancelled(),
                        "PostgreSQL readiness wait cancelled"
                    );
                    break;
                }
                Some(Ok(Ok(_))) => {}
                Some(Ok(Err(error))) => {
                    return Err(error).context("wait PostgreSQL target database probe");
                }
                _ => {
                    // kill() waits/reaps as well. Never start a migration after
                    // cancellation or an unconfirmed child cleanup.
                    anyhow::ensure!(
                        !cancel.is_cancelled(),
                        "PostgreSQL readiness wait cancelled"
                    );
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("PostgreSQL readiness wait cancelled"),
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + interval)) => {},
            }
        }
    }
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "PostgreSQL readiness wait cancelled"
    );
    info!("PostgreSQL target database login and SELECT 1 confirmed");
    Ok(())
}

/// 轮询单个桥接后端的 readiness_path 直至就绪(120s 超时)。
///
/// 仅在 workspace.manifest `[health].bridge_service` 显式配置时调用(只等那一个后端)。
/// 默认(不配 bridge)不调本函数 —— app-cli 自给 /ready,不强依赖任何后端。
pub(crate) async fn wait_for_service_ready(spec: &ServiceSpec) -> Result<()> {
    wait_for_service_ready_within(spec, 120).await
}

/// 在窗口（秒）内轮询端口 TCP 连通（devrun 热加载命令的就绪判定——HTTP 路径
/// 语义对 dev server 不可知，端口在听即就绪）。
pub(crate) async fn wait_for_port_open(spec: &ServiceSpec, timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", spec.port)).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "port {} not open within {} seconds (last connect: {e})",
                        spec.port,
                        timeout_secs
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 在给定窗口（秒）内轮询 readiness_path 至 2xx；超时返回 Err（含路径信息）。
/// bridge_service 等待（固定 120s）与启动逐服务探测（`[health].
/// startup_timeout_seconds`）共用同一探测核心。
pub(crate) async fn wait_for_service_ready_within(
    spec: &ServiceSpec,
    timeout_secs: u64,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build readiness HTTP client")?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        spec.port, spec.health.readiness_path
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let ready = client
            .get(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "readiness '{}' not ready within {} seconds (last probe: {})",
                url,
                timeout_secs,
                "no 2xx",
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

//! Official Pingap capabilities are observed from an owned temporary process.
//! The native `-t` command warns, rather than fails, on unavailable plugin
//! categories, so its success alone cannot validate a candidate graph.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use pingap_config::PingapConfig;
use process_utils::{guardian::OwnedChild, managed_tree::StopOutcome};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command, time::Instant};

use crate::devtool::{
    DEFAULT_PINGAP_COMMIT as PINNED_COMMIT, DEFAULT_PINGAP_VERSION as PINNED_VERSION,
};
const CLEANUP_GRACE: Duration = Duration::from_secs(3);
const MAX_BASIC_BYTES: usize = 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 32;
type CapabilitySet = Arc<BTreeSet<String>>;
type CapabilityCache = Mutex<BTreeMap<String, CapabilitySet>>;
static CACHE: OnceLock<CapabilityCache> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct BasicCapabilities {
    pid: String,
    version: String,
    git_hash: String,
    supported_plugins: Vec<String>,
}

/// Every stage shares the caller's deadline. Cancellation retains the exact
/// process handle and temporary resources until physical cleanup is confirmed.
pub(crate) async fn preflight_until(
    binary: &Path,
    config: &PingapConfig,
    deadline: Instant,
) -> Result<String> {
    require_budget(deadline, "proxy capability preflight")?;
    let digest = tokio::time::timeout_at(deadline, binary_digest(binary))
        .await
        .context("proxy capability digest deadline exceeded")??;
    let cached = cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy capability cache poisoned"))?
        .get(&digest)
        .cloned();
    let capabilities = match cached {
        Some(capabilities) => capabilities,
        None => {
            let capabilities = Arc::new(probe_owned(binary, deadline).await?);
            let after = tokio::time::timeout_at(deadline, binary_digest(binary))
                .await
                .context("proxy binary identity recheck deadline exceeded")??;
            anyhow::ensure!(
                after == digest,
                "proxy binary changed during capability preflight"
            );
            let mut cache = cache()
                .lock()
                .map_err(|_| anyhow::anyhow!("proxy capability cache poisoned"))?;
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.pop_first();
            }
            cache
                .entry(digest.clone())
                .or_insert_with(|| capabilities.clone());
            capabilities
        }
    };
    validate_plugins(config, &capabilities)?;
    require_budget(deadline, "proxy capability preflight")?;
    Ok(digest)
}

/// A cached capability observation must still name the executable that ran
/// native test mode. Check again after `-t` within the original budget.
pub(crate) async fn verify_binary_until(
    binary: &Path,
    expected_digest: &str,
    deadline: Instant,
) -> Result<()> {
    require_budget(deadline, "proxy binary recheck")?;
    let observed = tokio::time::timeout_at(deadline, binary_digest(binary))
        .await
        .context("proxy binary recheck deadline exceeded")??;
    anyhow::ensure!(
        observed == expected_digest,
        "proxy executable changed between capability preflight and native configuration test"
    );
    require_budget(deadline, "proxy binary recheck")?;
    Ok(())
}

fn require_budget(deadline: Instant, stage: &str) -> Result<()> {
    anyhow::ensure!(Instant::now() < deadline, "{stage} deadline exceeded");
    Ok(())
}

fn execution_deadline(deadline: Instant, stage: &str) -> Result<Instant> {
    deadline
        .checked_sub(CLEANUP_GRACE)
        .filter(|remaining| *remaining > Instant::now())
        .with_context(|| format!("{stage} budget cannot accommodate cleanup"))
}

fn cache() -> &'static CapabilityCache {
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

async fn binary_digest(binary: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(binary)
        .await
        .with_context(|| format!("read proxy executable {}", binary.display()))?;
    let mut hasher = Sha256::new();
    // This buffer survives an await. Keeping it inline makes every enclosing
    // preflight/startup future carry 64 KiB and overflows ordinary Tokio
    // worker stacks when those futures are constructed in the owner loop.
    let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        let length = file
            .read(&mut buffer)
            .await
            .context("hash proxy executable")?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn validate_plugins(config: &PingapConfig, supported: &BTreeSet<String>) -> Result<()> {
    for (name, plugin) in &config.plugins {
        let category = plugin
            .get("category")
            .and_then(toml::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("proxy plugin {name} has no valid category"))?;
        anyhow::ensure!(
            supported.contains(category),
            "proxy plugin {name} requires unavailable official Pingap category {category}"
        );
    }
    for (name, location) in &config.locations {
        for plugin in location.plugins.iter().flatten() {
            // These exact aliases are installed by the pinned official source
            // before user plugins. An arbitrary `pingap:` prefix is not proof
            // that a reference exists in the current executable.
            let builtin = match plugin.as_str() {
                "pingap:compression" | "pingap:compressionUpstream" => Some("compression"),
                "pingap:ping" => Some("ping"),
                "pingap:stats" => Some("stats"),
                "pingap:requestId" => Some("request_id"),
                "pingap:acceptEncodingAdjustment" => Some("accept_encoding"),
                "pingap:admin" => Some("admin"),
                _ => None,
            };
            anyhow::ensure!(
                config.plugins.contains_key(plugin)
                    || builtin.is_some_and(|category| supported.contains(category)),
                "proxy location {name} references missing plugin {plugin}"
            );
        }
    }
    Ok(())
}

struct ProbeLease {
    child: Option<OwnedChild>,
    directory: Option<tempfile::TempDir>,
}

impl ProbeLease {
    fn child(&mut self) -> Result<&mut OwnedChild> {
        self.child
            .as_mut()
            .context("proxy capability process handle missing")
    }

    async fn cleanup(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(crate::supervisor::ShutdownUnconfirmed(
                "temporary official Pingap cleanup deadline exceeded".into(),
            )
            .into());
        }
        let outcome =
            tokio::time::timeout_at(deadline, self.child()?.stop(CLEANUP_GRACE.min(remaining)))
                .await
                .map_err(|error| {
                    crate::supervisor::ShutdownUnconfirmed(format!(
                        "temporary official Pingap cleanup deadline exceeded: {error}"
                    ))
                })?;
        if outcome == StopOutcome::Unconfirmed {
            return Err(crate::supervisor::ShutdownUnconfirmed(
                "temporary official Pingap cleanup remains unconfirmed".into(),
            )
            .into());
        }
        self.child.take();
        self.directory.take();
        Ok(())
    }
}

impl Drop for ProbeLease {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            process_utils::command_context::retain_cleanup_with_resource(
                Some(child),
                None,
                self.directory.take(),
            );
        }
    }
}

async fn probe_owned(binary: &Path, deadline: Instant) -> Result<BTreeSet<String>> {
    let probe_deadline = execution_deadline(deadline, "proxy capability preflight")?;
    let directory = tempfile::Builder::new()
        .prefix("rcoder-pingap-capabilities-")
        .tempdir()
        .context("create private proxy capability directory")?;
    let config = PingapConfig {
        basic: pingap_config::BasicConf {
            grace_period: Some(Duration::ZERO),
            graceful_shutdown_timeout: Some(Duration::from_secs(1)),
            pid_file: Some(
                directory
                    .path()
                    .join("pingap.pid")
                    .to_string_lossy()
                    .into_owned(),
            ),
            upgrade_sock: Some(
                directory
                    .path()
                    .join("upgrade.sock")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    let config_path = directory.path().join("pingap.toml");
    tokio::time::timeout_at(
        probe_deadline,
        tokio::fs::write(&config_path, toml::to_string_pretty(&config)?),
    )
    .await
    .context("proxy capability configuration write deadline exceeded")?
    .context("write basic-only proxy capability configuration")?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .context("reserve loopback proxy capability port")?;
    let address = listener.local_addr()?;
    drop(listener);
    let user = uuid::Uuid::new_v4().simple().to_string();
    let password = uuid::Uuid::new_v4().simple().to_string();
    let mut command = Command::new(binary);
    super::official_env::sanitize(&mut command);
    command
        .arg("-c")
        .arg(&config_path)
        // No --cp: that upstream path cannot furnish this process observation.
        .env("PINGAP_ADMIN_ADDR", address.to_string())
        .env("PINGAP_ADMIN_USER", &user)
        .env("PINGAP_ADMIN_PASSWORD", &password)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut lease = spawn_lease(command, false, probe_deadline, deadline, directory)
        .await
        .context("start owned official Pingap capability process")?;
    let result = tokio::time::timeout_at(probe_deadline, async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(250))
            .timeout(Duration::from_secs(1))
            .build()
            .context("construct local proxy capability reader")?;
        loop {
            if let Some(status) = lease.child()?.try_wait_root()? {
                anyhow::bail!(
                    "official Pingap capability process exited before observation: {status}"
                );
            }
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock precedes Unix epoch")?
                .as_secs();
            let authorization =
                super::admin_probe::authorization_header(&user, &password, timestamp);
            if let Ok(response) = client
                .get(format!("http://{address}/api/basic"))
                .header(reqwest::header::AUTHORIZATION, authorization)
                .send()
                .await
            {
                anyhow::ensure!(
                    !matches!(response.status().as_u16(), 401 | 403),
                    "owned official Pingap capability authentication rejected (HTTP {})",
                    response.status()
                );
                if !response.status().is_success() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                let basic = read_basic(response).await?;
                // The guarded worker can answer before its Running receipt
                // becomes visible. Missing identity is not ownership evidence.
                let Some(pid) = lease.child()?.id() else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                anyhow::ensure!(
                    lease.child()?.try_wait_root()?.is_none(),
                    "proxy capability worker exited during observation"
                );
                validate_basic(&basic, pid)?;
                return Ok(basic.supported_plugins.into_iter().collect());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("official Pingap capability observation deadline exceeded")
    .and_then(|result| result);
    // Failure is never cached. Physical cleanup is mandatory even after a
    // valid basic response, and is retried using the retained exact handle.
    lease.cleanup(deadline).await?;
    result
}

async fn spawn_lease(
    command: Command,
    capture: bool,
    execution_deadline: Instant,
    cleanup_deadline: Instant,
    directory: tempfile::TempDir,
) -> Result<ProbeLease> {
    require_budget(execution_deadline, "temporary official Pingap startup")?;
    let scope = crate::supervisor::resident::owner_scope()?;
    // Dropping the caller while a guardian handshake is in flight must not
    // delete its configuration or lose the eventual exact child. This bounded
    // task owns both resources; an unconsumed result runs ProbeLease::drop and
    // retains cleanup. It never gains a deadline beyond this request's budget.
    process_utils::command_context::spawn_scoped(async move {
        require_budget(execution_deadline, "temporary official Pingap startup")?;
        let attempted = match scope {
            Some(scope) => {
                process_utils::guardian::spawn_guarded_owner_checked_until(
                    command,
                    &scope,
                    capture,
                    execution_deadline,
                )
                .await
            }
            None => {
                process_utils::guardian::spawn_owned_checked_until(
                    command,
                    None,
                    capture,
                    execution_deadline,
                )
                .await
            }
        };
        match attempted {
            Ok(child) => Ok(ProbeLease {
                child: Some(child),
                directory: Some(directory),
            }),
            Err(failure) => {
                let mut lease = ProbeLease {
                    child: failure.child,
                    directory: Some(directory),
                };
                if lease.child.is_some() {
                    lease.cleanup(cleanup_deadline).await?;
                }
                Err(failure.source)
            }
        }
    })
    .await
    .context("temporary official Pingap startup task failed")?
}

/// Candidate `-t` belongs to the same explicit resident owner as the temporary
/// capability probe; it cannot fall into a stopped business generation.
pub(crate) async fn native_test_until(
    mut command: Command,
    deadline: Instant,
    directory: tempfile::TempDir,
) -> Result<std::process::Output> {
    let execution_deadline = execution_deadline(deadline, "native proxy validation")?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut lease = spawn_lease(command, true, execution_deadline, deadline, directory)
        .await
        .context("native proxy validation process startup")?;
    let stdout = lease.child()?.take_stdout();
    let stderr = lease.child()?.take_stderr();
    let out = tokio::spawn(capture_bounded(stdout));
    let err = tokio::spawn(capture_bounded(stderr));
    let result = tokio::time::timeout_at(execution_deadline, lease.child()?.wait_root()).await;
    lease.cleanup(deadline).await?;
    let status = result.context("native proxy validation execution deadline exceeded")??;
    let stdout = tokio::time::timeout_at(deadline, out)
        .await
        .context("native proxy validation stdout deadline exceeded")???;
    let stderr = tokio::time::timeout_at(deadline, err)
        .await
        .context("native proxy validation stderr deadline exceeded")???;
    require_budget(deadline, "native proxy validation")?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

async fn capture_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut reader) = reader {
        let mut buffer = [0u8; 8192];
        loop {
            let length = reader.read(&mut buffer).await?;
            if length == 0 {
                break;
            }
            let keep = length.min(MAX_BASIC_BYTES.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..keep]);
        }
    }
    Ok(bytes)
}

async fn read_basic(mut response: reqwest::Response) -> Result<BasicCapabilities> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read proxy capability response")?
    {
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_BASIC_BYTES,
            "proxy capability response exceeded its size bound"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("decode official Pingap basic capabilities")
}

fn validate_basic(basic: &BasicCapabilities, expected_pid: u32) -> Result<()> {
    anyhow::ensure!(
        basic
            .pid
            .parse::<u32>()
            .context("invalid proxy capability PID")?
            == expected_pid,
        "proxy capability response belongs to another process"
    );
    anyhow::ensure!(
        basic.version == PINNED_VERSION && basic.git_hash == PINNED_COMMIT,
        "proxy capability process does not match the pinned official Pingap version and source"
    );
    anyhow::ensure!(
        !basic.supported_plugins.is_empty(),
        "official Pingap capability response has no supported plugins"
    );
    Ok(())
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pingap_config::PingapConfig;
use tokio::process::Command;
use workspace_manifest::{PingapMode, ReleaseLock};

use super::pingap::{PINGAP_PORT, ProxyEntry, build_pingap_config};

const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;
const MAX_OBJECTS_PER_CATEGORY: usize = 256;

pub async fn compile_and_validate(
    workspace: &Path,
    runtime_root: &Path,
    pingap_bin: &Path,
    release: &ReleaseLock,
) -> Result<PathBuf> {
    let mut config = match release.pingap.mode {
        PingapMode::Managed => managed_config(release)?,
        PingapMode::Extend => compile_extend(workspace, release).await?,
        PingapMode::Custom => load_user_config(workspace, release).await?,
    };
    resolve_service_addresses(&mut config, release)?;
    validate_guardrails(&config)?;
    config.validate().context("PingapConfig::validate")?;
    let content = toml::to_string_pretty(&config).context("serialize effective Pingap config")?;
    if content.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("effective Pingap config exceeds {MAX_CONFIG_BYTES} bytes");
    }

    let target_dir = runtime_root.join(&release.release_id);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .with_context(|| format!("create Pingap runtime dir {}", target_dir.display()))?;
    let temporary = target_dir.join("pingap.toml.tmp");
    let target = target_dir.join("pingap.toml");
    tokio::fs::write(&temporary, content)
        .await
        .with_context(|| format!("write Pingap config {}", temporary.display()))?;
    set_private_permissions(&temporary).await?;
    let output = Command::new(pingap_bin)
        .arg("-t")
        .arg("-c")
        .arg(&temporary)
        .output()
        .await
        .with_context(|| format!("execute {} -t", pingap_bin.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "pingap -t rejected config: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    tokio::fs::rename(&temporary, &target)
        .await
        .with_context(|| format!("commit Pingap config {}", target.display()))?;
    Ok(target)
}

fn managed_config(release: &ReleaseLock) -> Result<PingapConfig> {
    let entries: Vec<_> = release
        .services
        .iter()
        .filter_map(|service| {
            service.proxy.as_ref().map(|proxy| ProxyEntry {
                name: service.service_id.clone(),
                port: service.port,
                proxy: proxy.clone(),
                health: service.health.readiness_path.clone(),
            })
        })
        .collect();
    let content = build_pingap_config(&entries)?
        .ok_or_else(|| anyhow::anyhow!("workspace has no proxied web service"))?;
    PingapConfig::new(content.as_bytes(), true).context("parse managed Pingap config")
}

async fn compile_extend(workspace: &Path, release: &ReleaseLock) -> Result<PingapConfig> {
    let mut managed = managed_config(release)?;
    let extension = load_user_config(workspace, release).await?;
    if !extension.servers.is_empty()
        || !extension.locations.is_empty()
        || !extension.upstreams.is_empty()
        || !extension.certificates.is_empty()
    {
        anyhow::bail!(
            "extend mode only permits [plugins] and [storages]; topology remains platform-managed"
        );
    }
    merge_unique(&mut managed.plugins, extension.plugins, "plugin")?;
    merge_unique(&mut managed.storages, extension.storages, "storage")?;
    let known_plugins: BTreeSet<_> = managed.plugins.keys().map(String::as_str).collect();
    let known_storages: BTreeSet<_> = managed.storages.keys().map(String::as_str).collect();
    for service in &release.services {
        if let Some(proxy) = &service.proxy {
            for plugin in &proxy.plugins {
                if !plugin.starts_with("pingap:") && !known_plugins.contains(plugin.as_str()) {
                    anyhow::bail!(
                        "service {} references missing Pingap plugin {plugin}",
                        service.service_id
                    );
                }
            }
            for storage in &proxy.upstream_includes {
                if !known_storages.contains(storage.as_str()) {
                    anyhow::bail!(
                        "service {} references missing Pingap storage/include {storage}",
                        service.service_id
                    );
                }
            }
        }
    }
    Ok(managed)
}

async fn load_user_config(workspace: &Path, release: &ReleaseLock) -> Result<PingapConfig> {
    let relative = release
        .pingap
        .config
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Pingap config path is missing from release lock"))?;
    let path = workspace.join(relative);
    let canonical_workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize Pingap config {}", path.display()))?;
    if !canonical.starts_with(&canonical_workspace) {
        anyhow::bail!("Pingap config path escapes workspace");
    }
    let bytes = if canonical.is_dir() {
        pingap_config::read_all_config_files(&canonical.to_string_lossy())
            .await
            .context("read multi-file Pingap config")?
    } else {
        tokio::fs::read(&canonical)
            .await
            .with_context(|| format!("read Pingap config {}", canonical.display()))?
    };
    if bytes.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("Pingap source config exceeds {MAX_CONFIG_BYTES} bytes");
    }
    PingapConfig::new(&bytes, true).context("parse user Pingap config")
}

fn resolve_service_addresses(config: &mut PingapConfig, release: &ReleaseLock) -> Result<()> {
    let services: BTreeMap<_, _> = release
        .services
        .iter()
        .filter(|service| service.enabled)
        .map(|service| (service.service_id.as_str(), service))
        .collect();
    for (upstream_name, upstream) in &mut config.upstreams {
        for address in &mut upstream.addrs {
            let Some(service_id) = address.strip_prefix("rcoder://") else {
                continue;
            };
            if service_id.contains('/') || service_id.contains(':') {
                anyhow::bail!("invalid rcoder service URI in upstream {upstream_name}: {address}");
            }
            let service = services.get(service_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "upstream {upstream_name} references missing or disabled service {service_id}"
                )
            })?;
            *address = format!("127.0.0.1:{}", service.port);
        }
    }
    Ok(())
}

fn validate_guardrails(config: &PingapConfig) -> Result<()> {
    for (category, count) in [
        ("server", config.servers.len()),
        ("location", config.locations.len()),
        ("upstream", config.upstreams.len()),
        ("plugin", config.plugins.len()),
        ("certificate", config.certificates.len()),
        ("storage", config.storages.len()),
    ] {
        if count > MAX_OBJECTS_PER_CATEGORY {
            anyhow::bail!("{category} count exceeds {MAX_OBJECTS_PER_CATEGORY}");
        }
    }
    let required = format!("0.0.0.0:{PINGAP_PORT}");
    let mut has_public_entrypoint = false;
    for (name, server) in &config.servers {
        for address in server.addr.split(',').map(str::trim) {
            if address == required {
                has_public_entrypoint = true;
                if !config.certificates.is_empty()
                    || server.global_certificates.unwrap_or(false)
                    || server.tls_cipher_list.is_some()
                    || server.tls_ciphersuites.is_some()
                    || server.tls_min_version.is_some()
                    || server.tls_max_version.is_some()
                {
                    anyhow::bail!(
                        "TLS/certificates are forbidden on {required}; the platform edge terminates TLS"
                    );
                }
                continue;
            }
            if !(address.starts_with("127.0.0.1:") || address.starts_with("[::1]:")) {
                anyhow::bail!(
                    "Pingap server {name} listener {address} is forbidden; only {required} or loopback listeners are allowed"
                );
            }
        }
    }
    if !has_public_entrypoint {
        anyhow::bail!("Pingap config must expose exactly the platform entrypoint {required}");
    }
    for (name, upstream) in &config.upstreams {
        for address in &upstream.addrs {
            validate_upstream_destination(name, address)?;
        }
    }
    for (name, plugin) in &config.plugins {
        validate_plugin_paths(name, plugin)?;
    }
    for (field, value) in [
        ("basic.pid_file", config.basic.pid_file.as_deref()),
        ("basic.error_log", config.basic.error_log.as_deref()),
        ("basic.upgrade_sock", config.basic.upgrade_sock.as_deref()),
    ] {
        if let Some(value) = value {
            validate_runtime_path(field, value)?;
        }
    }
    Ok(())
}

fn validate_upstream_destination(name: &str, address: &str) -> Result<()> {
    let normalized = address.to_ascii_lowercase();
    let forbidden = [
        "169.254.",
        "[fe80:",
        "metadata.google.internal",
        "metadata.azure.internal",
        "100.100.100.200",
    ];
    if forbidden.iter().any(|prefix| normalized.contains(prefix)) {
        anyhow::bail!(
            "Pingap upstream {name} targets a forbidden metadata/link-local address: {address}"
        );
    }
    Ok(())
}

fn validate_plugin_paths(name: &str, plugin: &impl serde::Serialize) -> Result<()> {
    let value = serde_json::to_value(plugin)
        .with_context(|| format!("serialize Pingap plugin {name} for guardrail validation"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Pingap plugin {name} must be an object"))?;
    for (key, value) in object {
        if let Some(path) = value.as_str()
            && (key.contains("path") || key.contains("file") || key.contains("directory"))
            && path.starts_with('/')
        {
            validate_runtime_path(&format!("plugins.{name}.{key}"), path)?;
        }
    }
    Ok(())
}

fn validate_runtime_path(field: &str, value: &str) -> Result<()> {
    let allowed = ["/app/code", "/app/data", "/app/logs", "/run/app-cli"];
    if !allowed
        .iter()
        .any(|root| value == *root || value.starts_with(&format!("{root}/")))
    {
        anyhow::bail!(
            "{field} path is outside the allowed runtime roots (/app/code, /app/data, /app/logs, /run/app-cli): {value}"
        );
    }
    Ok(())
}

fn merge_unique<K, V>(
    target: &mut std::collections::HashMap<K, V>,
    source: std::collections::HashMap<K, V>,
    category: &str,
) -> Result<()>
where
    K: std::hash::Hash + Eq + std::fmt::Display,
{
    for (name, value) in source {
        if target.contains_key(&name) {
            anyhow::bail!("Pingap {category} name conflicts with managed config: {name}");
        }
        target.insert(name, value);
    }
    Ok(())
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

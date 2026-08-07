use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pingap_config::PingapConfig;
use tokio::process::Command;
use workspace_manifest::{PingapMode, ReleaseLock};

use super::pingap::{PINGAP_PORT, ProxyEntry, build_pingap_config};

const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;
const MAX_OBJECTS_PER_CATEGORY: usize = 256;

/// 编译产物：生效配置路径 + 期望 config_hash（供 admin 只读确认重载生效）。
pub struct CompileOutcome {
    pub config_path: PathBuf,
    pub expected_hash: String,
}

pub async fn compile_and_validate(
    workspace: &Path,
    runtime_root: &Path,
    pingap_bin: &Path,
    release: &ReleaseLock,
) -> Result<CompileOutcome> {
    let (content, expected_hash) = compile_effective_config(workspace, release).await?;

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
    // rename 前备份当前生效 TOML 为 pingap.toml.prev（保留上一份供 reload 失败回切）。
    if tokio::fs::try_exists(&target)
        .await
        .with_context(|| format!("stat Pingap config {}", target.display()))?
    {
        let backup = target_dir.join("pingap.toml.prev");
        tokio::fs::copy(&target, &backup)
            .await
            .with_context(|| format!("backup Pingap config {}", backup.display()))?;
    }
    tokio::fs::rename(&temporary, &target)
        .await
        .with_context(|| format!("commit Pingap config {}", target.display()))?;
    Ok(CompileOutcome {
        config_path: target,
        expected_hash,
    })
}

/// 编译 release lock 为生效 Pingap 配置 TOML —— 纯编译，不触碰文件系统、不调用 pingap 二进制。
///
/// mode 分发(managed/extend/custom)→ `rcoder://` 地址解析 → 护栏 + 语义校验 → hash → 序列化。
/// 供 `compile_and_validate`(运行时：再接 `pingap -t` + 原子落盘)与本地 `gen-lock` 预览共用，
/// 让"反向代理规则是否正确"能在不依赖 pingap 二进制的前提下验证。
///
/// 返回 `(toml_content, expected_hash)`。
pub async fn compile_effective_config(
    workspace: &Path,
    release: &ReleaseLock,
) -> Result<(String, String)> {
    let mut config = match release.pingap.mode {
        PingapMode::Managed => managed_config(release)?,
        PingapMode::Extend => compile_extend(workspace, release).await?,
        PingapMode::Custom => load_user_config(workspace, release).await?,
    };
    resolve_service_addresses(&mut config, release)?;
    validate_guardrails(&config)?;
    config.validate().context("PingapConfig::validate")?;
    // 期望 hash：与 pingap 加载同一 TOML 后 get_current_config().hash() 同算法
    //（descriptions 拼接 CRC32），供 reload 只读确认比对。
    let expected_hash = config
        .hash()
        .context("compute effective Pingap config hash")?;
    let content = toml::to_string_pretty(&config).context("serialize effective Pingap config")?;
    if content.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("effective Pingap config exceeds {MAX_CONFIG_BYTES} bytes");
    }
    Ok((content, expected_hash))
}

fn managed_config(release: &ReleaseLock) -> Result<PingapConfig> {
    let entries: Vec<_> = release
        .services
        .iter()
        // 防御过滤：release.lock 正常不含 disabled 服务，此为防御手工篡改/未来锁语义变化
        // （对齐 resolve_service_addresses 的 enabled 过滤先例）。
        .filter(|service| service.enabled)
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
    // 防御过滤：release.lock 正常不含 disabled 服务，此为防御手工篡改/未来锁语义变化；
    // disabled 服务的 plugin/storage 引用不参与校验（其拓扑也不会进入 managed 配置）。
    for service in release.services.iter().filter(|service| service.enabled) {
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

#[cfg(test)]
mod tests {
    use super::{managed_config, validate_upstream_destination};
    use workspace_manifest::ReleaseLock;

    fn release_lock_with_disabled_proxy() -> ReleaseLock {
        toml::from_str(
            r#"
schema_version = 1
release_id = "release-1"
workspace_name = "test"
minimum_app_cli_version = "0.1.0"
runtime_image_digest = "runtime:test"

[pingap]
mode = "managed"
version = "test"
commit = "test"

[[services]]
service_id = "api"
name = "API"
dir = "api"
type = "go"
kind = "web"
enabled = true
port = 18080

[services.run]
command = ["./api"]

[services.health]

[services.env]

[services.proxy]
path = "/api/"

[[services.logs]]
id = "application"
glob = "application*.log"
format = "text"

[[services]]
service_id = "worker"
name = "Worker"
dir = "worker"
type = "go"
kind = "web"
enabled = false
port = 18081

[services.run]
command = ["./worker"]

[services.health]

[services.env]

[services.proxy]
path = "/"

[[services.logs]]
id = "application"
glob = "application*.log"
format = "text"
"#,
        )
        .expect("valid release lock")
    }

    #[test]
    fn disabled_services_are_excluded_from_managed_config() {
        let config =
            managed_config(&release_lock_with_disabled_proxy()).expect("managed config compiles");
        assert!(config.upstreams.contains_key("api"));
        assert!(
            !config.upstreams.contains_key("worker"),
            "disabled service must not produce a Pingap upstream"
        );
        assert!(!config.locations.contains_key("workerLocation"));
    }

    #[test]
    fn disabled_only_proxy_services_yield_no_managed_config() {
        let mut release = release_lock_with_disabled_proxy();
        release
            .services
            .retain(|service| service.service_id == "worker");
        // 唯一的 proxied 服务被禁用 → 无拓扑可编译，报错而非生成空配置。
        assert!(managed_config(&release).is_err());
    }

    #[test]
    fn private_and_loopback_upstreams_are_allowed() {
        for address in [
            "10.0.0.8:8080",
            "192.168.32.229:8080",
            "172.20.1.8:8080",
            "127.0.0.1:8080",
            "service.namespace.svc.cluster.local:8080",
        ] {
            assert!(
                validate_upstream_destination("internal", address).is_ok(),
                "internal upstream should be allowed: {address}"
            );
        }
    }

    #[test]
    fn cloud_metadata_and_link_local_upstreams_remain_blocked() {
        for address in ["169.254.169.254:80", "metadata.google.internal:80"] {
            assert!(validate_upstream_destination("metadata", address).is_err());
        }
    }
}

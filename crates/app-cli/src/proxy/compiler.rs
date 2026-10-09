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

/// 进程级「当前期望生效 config_hash」槽：业务就绪观察用它核对 admin 实际
/// 生效 hash（不匹配 → PROXY_CONFIG_MISMATCH）。一个 app-cli 进程同一时刻
/// 只服务一个 release，进程级槽与该语义一致（同 admin endpoint 全局槽模式）。
///
/// 写入点：编排编译（builtin/supervisord）与 proxy reload 确认/回切确认——
/// 即「平台已确认过该 hash 生效」的位置；仅编译未确认不写入。
static EXPECTED_HASH: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 记录已确认生效的期望 config_hash（空串清除）。
pub fn record_expected_hash(hash: &str) {
    let mut guard = EXPECTED_HASH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if hash.is_empty() {
        *guard = None;
    } else {
        *guard = Some(hash.to_string());
    }
}

/// 当前已确认生效的期望 hash（None = 本进程尚未确认过任何配置）。
pub fn expected_hash() -> Option<String> {
    EXPECTED_HASH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Capture the execution profile when orchestration starts. Proxy reload must
/// use this profile, not a later request or the owner's startup environment.
#[derive(Clone)]
pub(crate) struct RuntimeProxyContext {
    pub workspace: PathBuf,
    pub dev_profile: bool,
}

pub(crate) fn runtime_root(log_root: &Path) -> PathBuf {
    std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| log_root.join("pingap"))
}

/// owner 生命周期固定的 active 配置路径（P1 常驻代理的 `-c` 目标）。
///
/// release 目录只保存候选/历史配置；发布 = 原子替换 active 内容。常驻
/// pingap（`--autoreload`）监视 active 路径，新 release 发布即热载——
/// 不再随会话重建进程。
pub fn active_config_path(runtime_root: &Path) -> PathBuf {
    runtime_root.join("active").join("pingap.toml")
}

/// 发布候选配置到 active 路径（原子：同目录 tmp + rename）。
pub async fn publish_active(runtime_root: &Path, candidate: &Path) -> Result<PathBuf> {
    let active = active_config_path(runtime_root);
    let parent = active
        .parent()
        .with_context(|| format!("active path {} has no parent", active.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create {}", parent.display()))?;
    let content = tokio::fs::read(candidate)
        .await
        .with_context(|| format!("read candidate {}", candidate.display()))?;
    let tmp = active.with_extension("toml.tmp");
    tokio::fs::write(&tmp, &content)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &active)
        .await
        .with_context(|| format!("publish active {}", active.display()))?;
    Ok(active)
}

/// standby 兜底配置（P1 摘流态）：全部路径 → mock 直出 503。
///
/// 停业务前发布并确认 standby——常驻入口不再把请求转发到已死 upstream
/// 产生裸 502。`publication_id` 进响应头与正文（诊断锚点；T11 引入
/// `/_pub/<id>` 探测路由前的过渡标记）。与生效配置同 hash 算法，供
/// admin 只读确认。
pub fn build_standby_config(publication_id: &str) -> Result<(String, String)> {
    build_standby_from_topology(publication_id, None)
}

/// C5：standby 从**当前 active 的真实拓扑**派生——保留全部 server 名字/
/// 监听地址/非 locations 字段（Custom 多 listener/自定义参数全部覆盖），
/// 仅把每个 server 的 locations 替换为兜底 mock。无 active（首编前的
/// 窗口）时退回单 listener 默认拓扑。
pub fn build_standby_from_topology(
    publication_id: &str,
    active: Option<&str>,
) -> Result<(String, String)> {
    use pingap_config::{LocationConf, PluginConf, ServerConf};
    let mut cfg = match active {
        Some(content) => PingapConfig::new(content.as_bytes(), true)
            .context("parse active config for standby derivation")?,
        None => PingapConfig::default(),
    };
    // 与生效配置同款热载节奏（2s 轮询）——standby 确认预算内必须可检测。
    cfg.basic.auto_restart_check_interval = Some(std::time::Duration::from_secs(2));
    if cfg.servers.is_empty() {
        cfg.servers.insert(
            "app".into(),
            ServerConf {
                addr: format!("0.0.0.0:{PINGAP_PORT}"),
                locations: Some(vec!["standby".into()]),
                ..Default::default()
            },
        );
    } else {
        // 每个既有 server 全部 locations → 单一 standby 兜底（清空业务
        // 路由：高权重前缀与兜底都被接管，无残留路径可旁路到死 upstream）。
        for server in cfg.servers.values_mut() {
            server.locations = Some(vec!["standby".into()]);
        }
    }
    // 清空业务 locations/upstreams/certificates 引用（standby 无 upstream；
    // certificates 属 TLS 终结配置——保留会让部分路径继续按旧证书服务？不：
    // mock 在 Request 阶段直出，证书只影响握手，保留与真实拓扑一致更稳）。
    cfg.locations.clear();
    cfg.upstreams.clear();
    cfg.locations.insert(
        "standby".into(),
        LocationConf {
            // 不写 path：默认权重 0 兜底，接管全部请求
            plugins: Some(vec!["standby".into()]),
            ..Default::default()
        },
    );
    // mock（Request 阶段直出，无 upstream）：status/headers/data
    let body =
        format!("service stopped or restarting (publication {publication_id}); retry shortly");
    let plugin: PluginConf = serde_json::from_str(
        &serde_json::json!({
            "category": "mock",
            "status": 503,
            "headers": [
                "Retry-After: 2",
                "Content-Type: text/plain; charset=utf-8",
                "Cache-Control: no-store",
                "X-Rcoder-Publication: ".to_string() + publication_id,
            ],
            "data": body,
        })
        .to_string(),
    )?;
    cfg.plugins.insert("standby".into(), plugin);
    cfg.validate().context("validate standby Pingap config")?;
    let expected_hash = cfg.hash().context("compute standby Pingap config hash")?;
    let content = toml::to_string_pretty(&cfg).context("serialize standby Pingap config")?;
    Ok((content, expected_hash))
}

/// 常驻热载兼容校验（P1 T15）：对比当前 active 与新候选的 server 拓扑。
///
/// pin（0.15.0）热载支持既有 server 的 locations/upstreams/plugins 更新；
/// **server 增删/改名/监听地址变化不可热载**——受理前 Fail Fast（保旧服务
/// 运行，指引走完整重配置流程），不得静默半生效。首编（active 不存在或
/// 无法解析）跳过校验（bootstrap 建立全量拓扑）。
pub fn validate_hot_reload_compatible(active: &Path, candidate: &str) -> Result<()> {
    let Ok(active_content) = std::fs::read_to_string(active) else {
        return Ok(()); // 无 active（首编）——bootstrap
    };
    // 与生成路径同构的宽松解析（PingapConfig::new 补全缺省段；严格
    // toml::from_str 会因缺 basic 等段拒绝合法的最小配置）。
    let active_cfg = PingapConfig::new(active_content.as_bytes(), true)
        .with_context(|| format!("parse active config {}", active.display()))?;
    let candidate_cfg = PingapConfig::new(candidate.as_bytes(), true).context("parse candidate")?;
    // C7：pin（0.15.0）热载支持 = 既有 server 的 locations 变化 + plugins +
    // upstreams。其余全部逐项比较：server 名单/监听/非 locations 字段、
    // storages、basic（热载语义未承诺的段落变化一律 Fail Fast）。
    // locations/upstreams/plugins 的**内容**差异放行（受支持类别）。
    // pingap-config 0.15.0 的 conf 类型未 derive PartialEq——经 serde 序列化
    // 成 serde_json::Value 比较（值语义等价，天然忽略 None vs 缺省差异）。
    let strip_locations = |mut server: pingap_config::ServerConf| {
        server.locations = None;
        server
    };
    let server_value = |server: &pingap_config::ServerConf| -> Result<serde_json::Value> {
        serde_json::to_value(strip_locations(server.clone()))
            .context("serialize server conf for compare")
    };
    let mut active_servers: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (name, server) in &active_cfg.servers {
        active_servers.insert(name.clone(), server_value(server)?);
    }
    let mut candidate_servers: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (name, server) in &candidate_cfg.servers {
        candidate_servers.insert(name.clone(), server_value(server)?);
    }
    if active_servers != candidate_servers {
        let added: Vec<_> = candidate_servers
            .keys()
            .filter(|name| !active_servers.contains_key(*name))
            .collect();
        let removed: Vec<_> = active_servers
            .keys()
            .filter(|name| !candidate_servers.contains_key(*name))
            .collect();
        let changed: Vec<_> = candidate_servers
            .iter()
            .filter(|(name, conf)| active_servers.get(*name).is_some_and(|old| old != *conf))
            .map(|(name, _)| name.clone())
            .collect();
        anyhow::bail!(
            "resident pingap hot-reload cannot apply this configuration change \
             (server topology differs: added={added:?} removed={removed:?} \
             conf-changed={changed:?}); use the full reconfiguration flow \
             instead of hot deploy"
        );
    }
    // storages / basic：整段比较。basic 里我们自己恒写
    // auto_restart_check_interval=2s，两侧同值不影响相等性；其余 basic
    // 字段（线程数/日志等）变化未承诺热载。
    let storages_value = |cfg: &PingapConfig| -> Result<serde_json::Value> {
        serde_json::to_value(&cfg.storages).context("serialize storages for compare")
    };
    if storages_value(&active_cfg)? != storages_value(&candidate_cfg)? {
        anyhow::bail!(
            "resident pingap hot-reload cannot apply storages changes \
             ({:?} -> {:?}); use the full reconfiguration flow",
            active_cfg.storages.keys().collect::<Vec<_>>(),
            candidate_cfg.storages.keys().collect::<Vec<_>>(),
        );
    }
    let basic_value = |cfg: &PingapConfig| -> Result<serde_json::Value> {
        serde_json::to_value(&cfg.basic).context("serialize basic for compare")
    };
    if basic_value(&active_cfg)? != basic_value(&candidate_cfg)? {
        anyhow::bail!(
            "resident pingap hot-reload cannot apply basic changes; \
             use the full reconfiguration flow"
        );
    }
    Ok(())
}

/// 编译并发布 standby 到 active（停业务前的摘流步骤）。返回期望 hash
/// （调用方经 admin 确认热载生效后才停止业务服务）。
pub async fn publish_standby(runtime_root: &Path, publication_id: &str) -> Result<String> {
    let active = active_config_path(runtime_root);
    let active_content = tokio::fs::read_to_string(&active).await.ok();
    let (content, expected_hash) =
        build_standby_from_topology(publication_id, active_content.as_deref())?;
    let candidate_dir = runtime_root.join("standby");
    tokio::fs::create_dir_all(&candidate_dir)
        .await
        .with_context(|| format!("create {}", candidate_dir.display()))?;
    let candidate = candidate_dir.join("pingap.toml");
    tokio::fs::write(&candidate, content)
        .await
        .with_context(|| format!("write {}", candidate.display()))?;
    publish_active(runtime_root, &candidate).await?;
    Ok(expected_hash)
}

pub async fn compile_and_validate(
    workspace: &Path,
    runtime_root: &Path,
    pingap_bin: &Path,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<CompileOutcome> {
    let runtime_layout_roots: Vec<PathBuf> = std::fs::canonicalize(runtime_root)
        .map(|root| vec![root])
        .unwrap_or_default();
    let (content, expected_hash) =
        compile_effective_config_with_roots(workspace, release, &runtime_layout_roots, dev_profile)
            .await?;

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
    let mut command = Command::new(pingap_bin);
    command.arg("-t").arg("-c").arg(&temporary);
    let output = process_utils::guardian::output_owned(command, std::time::Duration::from_secs(30))
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

/// 只读编译 release lock 为生效 Pingap 配置 TOML，不落盘、不调用 pingap 二进制。
/// custom/extend 会读取配置；原生 Pingap 校验可能解析显式 static upstream 的主机名。
///
/// mode 分发(managed/extend/custom)→ `rcoder://` 地址解析 → 护栏 + 语义校验 → hash → 序列化。
/// 供 `compile_and_validate`(运行时：再接 `pingap -t` + 原子落盘)与本地 `gen-lock` 预览共用，
/// 让"反向代理规则是否正确"能在不依赖 pingap 二进制的前提下验证。
///
/// 返回 `(toml_content, expected_hash)`。
pub async fn compile_effective_config(
    workspace: &Path,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<(String, String)> {
    compile_effective_config_with_roots(workspace, release, &[], dev_profile).await
}

/// Configuration inspection shares every compilation rule, with a bounded native
/// validation observation. Blocking resolver work may outlive this deadline;
/// the short-lived validate executor must not wait for it during shutdown.
pub async fn compile_effective_config_for_inspection(
    workspace: &Path,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<(String, String)> {
    compile_effective_config_core(workspace, release, &[], dev_profile, true).await
}

/// [`compile_effective_config`] 的可参数化核心（N02）：`extra_layout_roots`
/// 为运行时布局根（pingap 运行目录等），与 workspace 规范化根并集做
/// 路径护栏校验——三平台一致，不再按平台整段跳过。
pub async fn compile_effective_config_with_roots(
    workspace: &Path,
    release: &ReleaseLock,
    extra_layout_roots: &[PathBuf],
    dev_profile: bool,
) -> Result<(String, String)> {
    compile_effective_config_core(workspace, release, extra_layout_roots, dev_profile, false).await
}

async fn compile_effective_config_core(
    workspace: &Path,
    release: &ReleaseLock,
    extra_layout_roots: &[PathBuf],
    dev_profile: bool,
    bounded_native_validation: bool,
) -> Result<(String, String)> {
    workspace_manifest::validate_release_startup(release)?;
    let mut layout_roots: Vec<PathBuf> = Vec::new();
    if let Ok(canonical) = std::fs::canonicalize(workspace) {
        layout_roots.push(canonical);
    }
    layout_roots.extend(extra_layout_roots.iter().cloned());
    let mut config = match release.pingap.mode {
        PingapMode::Managed => managed_config(workspace, release, dev_profile)?,
        PingapMode::Extend => compile_extend(workspace, release, dev_profile).await?,
        PingapMode::Custom => load_user_config(workspace, release).await?,
    };
    resolve_service_addresses(&mut config, release)?;
    validate_guardrails(&config, &layout_roots)?;
    if bounded_native_validation {
        let validation = tokio::task::spawn_blocking(move || {
            config.validate().context("PingapConfig::validate")?;
            Ok::<_, anyhow::Error>(config)
        });
        config = match tokio::time::timeout(std::time::Duration::from_secs(10), validation).await {
            Ok(result) => result.map_err(std::io::Error::other)??,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "native Pingap configuration validation exceeded 10 seconds",
                )
                .into());
            }
        };
    } else {
        config.validate().context("PingapConfig::validate")?;
    }
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

fn managed_config(
    workspace: &Path,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<PingapConfig> {
    let entries: Vec<_> = release
        .services
        .iter()
        // 防御过滤：release.lock 正常不含 disabled 服务，此为防御手工篡改/未来锁语义变化
        // （对齐 resolve_service_addresses 的 enabled 过滤先例）。
        .filter(|service| service.enabled)
        .filter_map(|service| {
            service.proxy.as_ref().map(|proxy| {
                let mut proxy = proxy.clone();
                proxy.strip_prefix =
                    proxy.effective_strip_prefix(dev_profile && service.devrun.is_some());
                ProxyEntry {
                    name: service.service_id.clone(),
                    port: service.port,
                    proxy,
                    health: service.health.readiness_path.clone(),
                }
            })
        })
        .collect();
    // workspace 首页兜底路由（index.html 存在且无 catch-all 服务时注入；
    // 判定单一事实源 workspace_index::index_port_if_eligible——运行时与
    // gen-lock 预览同一结论）
    let index_port = crate::workspace_index::index_port_if_eligible(workspace, &release.services);
    let content = build_pingap_config(&entries, index_port)?.ok_or_else(|| {
        anyhow::anyhow!(
            "workspace has no proxied web service: none of the enabled services declares a [proxy] section\n     fix:   pick the service that serves HTTP and add, in its project.manifest.toml:\n            [proxy]\n            path = \"/api/<service_id>/\"\n            strip_prefix = true"
        )
    })?;
    PingapConfig::new(content.as_bytes(), true).context("parse managed Pingap config")
}

async fn compile_extend(
    workspace: &Path,
    release: &ReleaseLock,
    dev_profile: bool,
) -> Result<PingapConfig> {
    let mut managed = managed_config(workspace, release, dev_profile)?;
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
    let canonical_workspace = tokio::fs::canonicalize(workspace)
        .await
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))?;
    let canonical = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("canonicalize Pingap config {}", path.display()))?;
    if !canonical.starts_with(&canonical_workspace) {
        anyhow::bail!("Pingap config path escapes workspace");
    }
    let source = super::config_source::read_config(&canonical, Path::new(relative)).await?;
    if source.bytes.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("Pingap source config exceeds {MAX_CONFIG_BYTES} bytes");
    }
    source.parse().context("parse user Pingap config")
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
            anyhow::ensure!(
                service.health.startup_probe != Some(workspace_manifest::StartupProbe::Process),
                "upstream {upstream_name} must not reference process worker {service_id}"
            );
            *address = format!("127.0.0.1:{}", service.port);
        }
    }
    Ok(())
}

/// N02：`layout_roots` = 本次编译的实际布局根（workspace / pingap 运行
/// 目录的规范化路径）——与容器字面量根并集做路径校验，原生平台不整段跳过。
fn validate_guardrails(config: &PingapConfig, layout_roots: &[std::path::PathBuf]) -> Result<()> {
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
        validate_plugin_paths_with_roots(name, plugin, layout_roots)?;
    }
    for (field, value) in [
        ("basic.pid_file", config.basic.pid_file.as_deref()),
        ("basic.error_log", config.basic.error_log.as_deref()),
        ("basic.upgrade_sock", config.basic.upgrade_sock.as_deref()),
    ] {
        if let Some(value) = value {
            validate_runtime_path(field, value, layout_roots)?;
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

/// 便捷入口（容器字面量根；生产路径走 [`validate_plugin_paths_with_roots`]
/// 带实际布局根）——测试与文档用途保留。
#[cfg(test)]
fn validate_plugin_paths(name: &str, plugin: &impl serde::Serialize) -> Result<()> {
    validate_plugin_paths_with_roots(name, plugin, &[])
}

/// [`validate_plugin_paths`] 的可参数化核心——`extra_roots` 为本次编译的
/// 实际布局根（workspace、pingap 运行目录的规范化路径），与容器字面量
/// 根并集校验（N02）。
fn validate_plugin_paths_with_roots(
    name: &str,
    plugin: &impl serde::Serialize,
    extra_roots: &[std::path::PathBuf],
) -> Result<()> {
    let value = serde_json::to_value(plugin)
        .with_context(|| format!("serialize Pingap plugin {name} for guardrail validation"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Pingap plugin {name} must be an object"))?;
    // pingap 的 `path`/`token_path` 等字段语义随 plugin category 不同:
    //   - mock/ping/stats/admin/cors/sub_filter/csrf 等:`path` 是 URL 请求路径或匹配正则
    //     (mock.rs 注释明写 "The URL path to match against incoming requests"),不是文件
    //     系统路径 —— 笼统当文件路径校验会误伤(实测 mock path="/healthz" 被拒,导致
    //     extend/custom 无法用 mock/ping/stats 等常见 plugin)。
    //   - directory:`path` 是文件系统根目录(directory.rs `path: PathBuf`),必须校验防穿越。
    // 因此对 `path` 类字段,仅当 category 属于文件类时校验;`file`/`directory`/`cert` 等
    // 字段名语义明确为文件,始终校验(如 cache.directory 缓存目录)。
    const FILE_PATH_CATEGORIES: &[&str] = &["directory"];
    let category = object
        .get("category")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    for (key, value) in object {
        let Some(path) = value.as_str() else { continue };
        // 绝对样式值才可能是文件路径（相对值/URL 值不在此判定）：
        // has_root 跨平台（Unix='/' 前缀；Windows=盘符或根相对 '\'/'/'）
        if !std::path::Path::new(path).has_root() {
            continue;
        }
        let is_file_field =
            key.contains("file") || key.contains("directory") || key.contains("cert");
        let is_path_field = key.contains("path");
        if is_file_field || (is_path_field && FILE_PATH_CATEGORIES.contains(&category)) {
            validate_runtime_path(&format!("plugins.{name}.{key}"), path, extra_roots)?;
        }
    }
    Ok(())
}

/// 运行时布局允许根的字面量部分（容器契约根；原生平台为惰性超集——
/// 目录不存在即无从逃逸，真实布局根由调用方以 extra_roots 追加）。
const CONTAINER_RUNTIME_ROOTS: [&str; 4] = ["/app/code", "/app/data", "/app/logs", "/run/app-cli"];

fn validate_runtime_path(
    field: &str,
    value: &str,
    extra_roots: &[std::path::PathBuf],
) -> Result<()> {
    // N02：路径校验不再按平台整段跳过——Windows 原生同样需要防配置逃逸。
    // 组件化前缀比较（Path::starts_with）在三个平台语义一致，正确处理
    // 分隔符差异（"/app/data/x" 与 "C:\app\data\x" 各按本平台规则比较）。
    let path = std::path::Path::new(value);
    let matched = CONTAINER_RUNTIME_ROOTS.iter().any(|root| {
        let root = std::path::Path::new(root);
        // Windows 上 "/app/..." 无盘符非绝对，但组件比较仍按根相对路径语义匹配
        path.starts_with(root)
    }) || extra_roots.iter().any(|root| path.starts_with(root));
    if !matched {
        let mut roots_desc = CONTAINER_RUNTIME_ROOTS.join(", ");
        for root in extra_roots {
            roots_desc.push_str(", ");
            roots_desc.push_str(&root.display().to_string());
        }
        anyhow::bail!("{field} path is outside the allowed runtime roots ({roots_desc}): {value}");
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
    use super::super::pingap::PINGAP_PORT;
    use super::{
        active_config_path, build_standby_config, managed_config, publish_active, publish_standby,
        validate_hot_reload_compatible, validate_plugin_paths, validate_upstream_destination,
    };
    use workspace_manifest::ReleaseLock;

    /// 无 index.html 的临时 workspace（兜底路由不注入的基线形态）。
    fn no_index_workspace() -> std::path::PathBuf {
        tempfile::tempdir().expect("tempdir").keep()
    }

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
        let config = managed_config(
            &no_index_workspace(),
            &release_lock_with_disabled_proxy(),
            false,
        )
        .expect("managed config compiles");
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
        assert!(managed_config(&no_index_workspace(), &release, false).is_err());
    }

    #[tokio::test]
    async fn dev_proxy_override_applies_only_to_services_running_devrun() {
        let workspace = tempfile::tempdir().unwrap();
        let mut release = release_lock_with_disabled_proxy();
        release.services.retain(|service| service.enabled);
        let service = &mut release.services[0];
        service.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["vite".into()],
        });
        let proxy = service.proxy.as_mut().unwrap();
        proxy.path = "/react".into();
        proxy.strip_prefix = true;
        proxy.dev_strip_prefix = Some(false);

        // Exercise the shared compiler used by startup, reload and gen-lock.
        for mode in [
            workspace_manifest::PingapMode::Managed,
            workspace_manifest::PingapMode::Extend,
        ] {
            release.pingap.mode = mode;
            if release.pingap.mode == workspace_manifest::PingapMode::Extend {
                std::fs::write(workspace.path().join("extension.toml"), "").unwrap();
                release.pingap.config = Some("extension.toml".into());
            }
            for (dev, has_devrun, override_value, should_strip) in [
                (true, true, Some(false), false),
                (false, true, Some(false), true),
                (true, false, Some(false), true),
                (true, true, None, true),
                (true, true, Some(true), true),
            ] {
                release.services[0].devrun =
                    has_devrun.then(|| workspace_manifest::DevrunSection {
                        command: vec!["vite".into()],
                    });
                release.services[0].proxy.as_mut().unwrap().dev_strip_prefix = override_value;
                let (content, _) = super::compile_effective_config(workspace.path(), &release, dev)
                    .await
                    .unwrap();
                let config = pingap_config::PingapConfig::new(content.as_bytes(), true).unwrap();
                let location = &config.locations["apiLocation"];
                assert_eq!(location.path.as_deref(), Some("/react"));
                assert_eq!(
                    location.rewrite.is_some(),
                    should_strip,
                    "dev={dev}, devrun={has_devrun}, override={override_value:?}"
                );
            }
        }
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

    #[test]
    fn url_path_plugin_fields_are_not_treated_as_file_paths() {
        // mock/ping/stats 的 path 是 URL 请求路径,pingap/csrf 的 token_path 同理,
        // 不应被文件路径护栏误伤(回归:曾因笼统 key.contains("path") 拒绝 mock plugin)。
        let mock = serde_json::json!({
            "category": "mock",
            "path": "/api/go/mocktest",
            "data": "{}",
            "status": 200,
        });
        assert!(validate_plugin_paths("go-mock", &mock).is_ok());
        let ping = serde_json::json!({"category": "ping", "path": "/ping"});
        assert!(validate_plugin_paths("ping", &ping).is_ok());
        let stats = serde_json::json!({"category": "stats", "path": "/stats"});
        assert!(validate_plugin_paths("stats", &stats).is_ok());
        let csrf = serde_json::json!({"category": "csrf", "token_path": "/csrf_token"});
        assert!(validate_plugin_paths("csrf", &csrf).is_ok());
    }

    #[test]
    fn directory_plugin_path_is_validated_against_runtime_roots() {
        // directory 的 path 是文件系统根目录(PathBuf),仅此 category 的 path 需校验防穿越。
        let ok = serde_json::json!({"category": "directory", "path": "/app/data/static"});
        assert!(validate_plugin_paths("dir-ok", &ok).is_ok());
        let bad = serde_json::json!({"category": "directory", "path": "/etc/passwd"});
        assert!(validate_plugin_paths("dir-bad", &bad).is_err());
    }

    /// P1：standby 兜底配置——全部路径 mock 直出 503 + 发布标记 + 2s 热载轮询。
    /// P1 T15：热载兼容校验——server 拓扑变化 Fail Fast，业务变化放行。
    #[test]
    fn hot_reload_guard_rejects_server_topology_change_only() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("active.toml");

        // 无 active（首编 bootstrap）：放行
        let candidate = r#"[basic]

[servers.app]
addr = "0.0.0.0:9080"
"#;
        assert!(validate_hot_reload_compatible(&active, candidate).is_ok());

        // 建立 active（单 server app）
        std::fs::write(&active, candidate).unwrap();
        // 同拓扑（仅 plugins 类别差异）：放行
        let same_topology = r#"[basic]

[servers.app]
addr = "0.0.0.0:9080"

[plugins.extra]
category = "mock"
"#;
        let ok = validate_hot_reload_compatible(&active, same_topology);
        assert!(ok.is_ok(), "{ok:?}");

        // server 改名/删除：拒绝
        let renamed = r#"[basic]

[servers.app2]
addr = "0.0.0.0:9080"
"#;
        let error = validate_hot_reload_compatible(&active, renamed)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("added=[\"app2\"]") && error.contains("removed=[\"app\"]"),
            "error must name the topology diff: {error}"
        );

        // 监听地址变化：拒绝
        let moved = r#"[basic]

[servers.app]
addr = "0.0.0.0:9081"
"#;
        assert!(
            validate_hot_reload_compatible(&active, moved)
                .unwrap_err()
                .to_string()
                .contains("conf-changed=[\"app\"]")
        );
    }

    #[test]
    fn standby_config_is_valid_mock_503_with_publication_marker() {
        let (content, hash) = build_standby_config("pub-abc123").expect("standby config");
        assert!(!hash.is_empty());
        let cfg: pingap_config::PingapConfig =
            toml::from_str(&content).expect("standby config parses");
        let server = cfg.servers.get("app").expect("server app");
        assert_eq!(server.addr, format!("0.0.0.0:{PINGAP_PORT}"));
        assert_eq!(
            server.locations.as_deref(),
            Some(&["standby".to_string()][..]),
            "standby must be the only location (covers all paths)"
        );
        let location = cfg.locations.get("standby").expect("standby location");
        assert!(location.path.is_none(), "no path = catch-all (weight 0)");
        assert!(location.upstream.is_none(), "standby has no upstream");
        let plugin = cfg.plugins.get("standby").expect("standby plugin");
        assert_eq!(
            plugin.get("category").and_then(|v| v.as_str()),
            Some("mock")
        );
        assert_eq!(
            plugin.get("status").and_then(|v| v.as_integer()),
            Some(503),
            "standby must return 503, not a bare 502 from dead upstream"
        );
        let headers = plugin
            .get("headers")
            .and_then(|v| v.as_array())
            .expect("headers array");
        let joined = format!("{headers:?}");
        assert!(
            joined.contains("Retry-After"),
            "Retry-After required: {joined}"
        );
        assert!(
            joined.contains("X-Rcoder-Publication"),
            "publication marker required: {joined}"
        );
        // pin（0.15.0 文件模式）周期轮询默认 90s——必须收紧到确认预算内
        assert_eq!(
            cfg.basic.auto_restart_check_interval,
            Some(std::time::Duration::from_secs(2))
        );
    }

    /// P1：active 发布——候选内容原子替换 active，候选保留。
    #[tokio::test]
    async fn publish_active_replaces_content_atomically_and_keeps_candidate() {
        let root = tempfile::tempdir().unwrap();
        let runtime_root = root.path().join("pingap");
        let candidate_dir = runtime_root.join("rel-1");
        tokio::fs::create_dir_all(&candidate_dir).await.unwrap();
        let candidate = candidate_dir.join("pingap.toml");
        tokio::fs::write(&candidate, "# release 1").await.unwrap();

        let active = publish_active(&runtime_root, &candidate).await.unwrap();
        assert_eq!(active, active_config_path(&runtime_root));
        assert_eq!(
            tokio::fs::read_to_string(&active).await.unwrap(),
            "# release 1"
        );
        assert!(
            candidate.exists(),
            "release candidate must be preserved as history"
        );

        // 二次发布（release 2）原子替换
        let candidate2 = runtime_root.join("rel-2").join("pingap.toml");
        tokio::fs::create_dir_all(candidate2.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&candidate2, "# release 2").await.unwrap();
        publish_active(&runtime_root, &candidate2).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(active_config_path(&runtime_root))
                .await
                .unwrap(),
            "# release 2"
        );
    }

    /// P1：publish_standby——编译 standby + 发布 active，返回可确认的 hash。
    #[tokio::test]
    async fn publish_standby_writes_active_and_returns_hash() {
        let root = tempfile::tempdir().unwrap();
        let runtime_root = root.path().join("pingap");
        let hash = publish_standby(&runtime_root, "pub-t1").await.unwrap();
        assert!(!hash.is_empty());
        let active = tokio::fs::read_to_string(active_config_path(&runtime_root))
            .await
            .unwrap();
        assert!(active.contains("mock"), "active must be the standby config");
    }

    #[test]
    fn explicit_file_fields_are_always_validated_regardless_of_category() {
        // file/directory/cert 等明确文件字段始终校验,不论 category。
        let cache_ok = serde_json::json!({"category": "cache", "directory": "/app/data/cache"});
        assert!(validate_plugin_paths("cache-ok", &cache_ok).is_ok());
        let cache_bad = serde_json::json!({"category": "cache", "directory": "/etc"});
        assert!(validate_plugin_paths("cache-bad", &cache_bad).is_err());
    }
}

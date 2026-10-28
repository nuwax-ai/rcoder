use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use crate::startup_probe::{StartupContext, StartupHost};
use anyhow::{Context, Result};
use pingap_config::PingapConfig;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use workspace_manifest::{PingapMode, ReleaseLock};

use super::pingap::{PINGAP_PORT, ProxyEntry, build_pingap_config};

const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;
const MAX_OBJECTS_PER_CATEGORY: usize = 256;

/// 编译产物：生效配置路径 + 期望 config_hash（供 admin 只读确认重载生效）。
#[derive(Clone, Debug)]
pub struct CompileOutcome {
    pub config_path: PathBuf,
    pub expected_hash: String,
    pub publication_id: String,
    /// Parent runtime operation when supplied; otherwise this publication is a local child operation.
    pub operation_id: Option<String>,
    /// Business release bound while preparing this immutable candidate. Old
    /// snapshots without this evidence must not borrow a newer desired release.
    pub serving_release_id: Option<String>,
    pub config_digest: String,
    pub entry_probes: Vec<ProxyProbeTarget>,
    pub business_probes: Vec<ServiceProbeTarget>,
}

#[derive(Clone, Debug)]
pub struct ProxyProbeTarget {
    pub address: std::net::SocketAddr,
    pub expected_status: u16,
}

#[derive(Clone, Debug)]
pub struct ServiceProbeTarget {
    pub service_id: String,
    pub address: std::net::SocketAddr,
    pub path: String,
}

#[cfg(test)]
pub(crate) fn fixture_publication_config(
    config: PingapConfig,
    operation: &str,
    status: u16,
) -> Result<PingapConfig> {
    publication_config(config, operation, status)
}

#[cfg(test)]
pub(crate) async fn fixture_persist_candidate(
    root: &Path,
    config: &PingapConfig,
    operation: &str,
    status: u16,
) -> Result<CompileOutcome> {
    persist_candidate(root, config, operation, status).await
}

pub(super) const PUBLICATION_OBJECT: &str = "rcoder:publication";

pub fn configuration_digest(config: &PingapConfig) -> Result<String> {
    let mut value = serde_json::to_value(config)?;
    value.sort_all_objects();
    let canonical = serde_json::to_vec(&value)?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

pub(super) fn publication_config(
    mut config: PingapConfig,
    publication: &str,
    status: u16,
) -> Result<PingapConfig> {
    anyhow::ensure!(
        uuid::Uuid::parse_str(publication).is_ok(),
        "invalid proxy publication UUID"
    );
    anyhow::ensure!(
        !config.plugins.contains_key(PUBLICATION_OBJECT)
            && !config.locations.contains_key(PUBLICATION_OBJECT),
        "configuration uses the reserved platform publication name"
    );
    let plugin = serde_json::json!({
        "category": "mock", "status": status, "data": publication,
        "headers": [format!("X-Rcoder-Publication: {publication}"), "Cache-Control: no-store".to_string()]
    });
    config
        .plugins
        .insert(PUBLICATION_OBJECT.into(), serde_json::from_value(plugin)?);
    config.locations.insert(
        PUBLICATION_OBJECT.into(),
        pingap_config::LocationConf {
            path: Some(format!("= /_pub/{publication}")),
            plugins: Some(vec![PUBLICATION_OBJECT.into()]),
            weight: Some(u16::MAX),
            ..Default::default()
        },
    );
    for server in config.servers.values_mut() {
        server
            .locations
            .get_or_insert_with(Vec::new)
            .insert(0, PUBLICATION_OBJECT.into());
    }
    Ok(config)
}

fn probe_targets(config: &PingapConfig, status: u16) -> Result<Vec<ProxyProbeTarget>> {
    let mut targets = BTreeSet::new();
    for server in config.servers.values() {
        for listener in server.addr.split(',') {
            let mut address: std::net::SocketAddr = listener.trim().parse().with_context(|| {
                format!("publication probe requires an explicit socket listener: {listener}")
            })?;
            if address.ip().is_unspecified() {
                address.set_ip(match address.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            targets.insert(address);
        }
    }
    anyhow::ensure!(
        !targets.is_empty(),
        "proxy publication has no listener to verify"
    );
    Ok(targets
        .into_iter()
        .map(|address| ProxyProbeTarget {
            address,
            expected_status: status,
        })
        .collect())
}

/// 进程级「当前期望生效 config_hash」槽：业务就绪观察用它核对 admin 实际
/// 生效 hash（不匹配 → PROXY_CONFIG_MISMATCH）。一个 app-cli 进程同一时刻
/// 只服务一个 release，进程级槽与该语义一致（同 admin endpoint 全局槽模式）。
///
/// 写入点：编排编译（builtin/supervisord）与 proxy reload 确认/回切确认——
/// 即「平台已确认过该 hash 生效」的位置；仅编译未确认不写入。
static EXPECTED_HASH: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static PUBLICATION_UNCERTAIN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn publication_uncertain() -> bool {
    PUBLICATION_UNCERTAIN.load(std::sync::atomic::Ordering::Acquire)
}
static PUBLICATION_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub async fn publication_guard() -> tokio::sync::MutexGuard<'static, ()> {
    PUBLICATION_GATE.lock().await
}
#[derive(Debug)]
pub struct ProxyOperationBusy {
    pub current: Option<shared_types::ProxyConfirmation>,
}

impl std::fmt::Display for ProxyOperationBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("proxy operation is already in progress")
    }
}
impl std::error::Error for ProxyOperationBusy {}

pub(crate) fn proxy_busy_error() -> anyhow::Error {
    ProxyOperationBusy {
        current: official_confirmation(),
    }
    .into()
}

pub(crate) fn try_publication_guard() -> Result<tokio::sync::MutexGuard<'static, ()>> {
    PUBLICATION_GATE.try_lock().map_err(|_| proxy_busy_error())
}
static OFFICIAL_CONFIRMATION: std::sync::Mutex<Option<shared_types::ProxyConfirmation>> =
    std::sync::Mutex::new(None);

pub fn official_confirmation() -> Option<shared_types::ProxyConfirmation> {
    OFFICIAL_CONFIRMATION
        .lock()
        .ok()
        .and_then(|value| value.clone())
}

pub(crate) fn record_official_phase(
    outcome: &CompileOutcome,
    phase: shared_types::ProxyPhase,
    instance: Option<&str>,
) -> Result<()> {
    let confirmed = matches!(
        phase,
        shared_types::ProxyPhase::Serving | shared_types::ProxyPhase::Standby
    );
    let view = shared_types::ProxyConfirmation {
        contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
        confirmed,
        phase,
        operation_id: outcome
            .operation_id
            .clone()
            .unwrap_or_else(|| outcome.publication_id.clone()),
        proxy_instance_id: instance.map(str::to_owned),
        config_digest: Some(outcome.config_digest.clone()),
    };
    let changed = {
        let mut held = OFFICIAL_CONFIRMATION
            .lock()
            .map_err(|_| anyhow::anyhow!("official proxy confirmation lock poisoned"))?;
        let changed = held.as_ref() != Some(&view);
        *held = Some(view.clone());
        changed
    };
    if changed {
        // Event bridges can read confirmation again. Release the state lock
        // before calling them, and keep credentials/configuration out of logs.
        tracing::info!(?phase, operation_id = %view.operation_id,
            "official proxy transition");
        crate::orchestration_events::emit(&crate::orchestration_events::OrchestrationEvent::Log {
            service: "pingap".into(),
            line: format!("Proxy {phase:?} (operation {})", view.operation_id),
        });
    }
    Ok(())
}
static CONFIRMED_PUBLICATION: std::sync::Mutex<
    Option<(CompileOutcome, super::apply_status::ConfirmedPublication)>,
> = std::sync::Mutex::new(None);

pub fn record_confirmed_publication(
    outcome: &CompileOutcome,
    confirmation: &super::apply_status::ConfirmedPublication,
) -> Result<()> {
    anyhow::ensure!(
        outcome.publication_id == confirmation.publication_id
            && outcome.operation_id == confirmation.operation_id
            && outcome.config_digest == confirmation.config_digest
            && super::admin_probe::hashes_match(&outcome.expected_hash, &confirmation.config_hash),
        "proxy confirmation does not belong to this candidate"
    );
    *CONFIRMED_PUBLICATION
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy publication lock poisoned"))? =
        Some((outcome.clone(), confirmation.clone()));
    let phase = if !outcome.entry_probes.is_empty()
        && outcome
            .entry_probes
            .iter()
            .all(|target| target.expected_status == 503)
    {
        shared_types::ProxyPhase::Standby
    } else {
        shared_types::ProxyPhase::Serving
    };
    record_official_phase(outcome, phase, Some(&confirmation.instance_id))?;
    record_expected_hash(&outcome.expected_hash);
    PUBLICATION_UNCERTAIN.store(false, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// A separate record preserves the official confirmation without extending
/// the strict historical engine receipt. This is app-cli process/HTTP evidence.
pub(crate) fn prepare_official_confirmation(
    root: &Path,
    outcome: &CompileOutcome,
    confirmation: &super::apply_status::ConfirmedPublication,
) -> Result<PreparedOfficialConfirmation> {
    #[derive(serde::Serialize)]
    struct Record<'a> {
        schema_version: u32,
        contract: &'static str,
        config_path: &'a Path,
        confirmation: &'a super::apply_status::ConfirmedPublication,
    }
    std::fs::create_dir_all(root).context("create official proxy confirmation directory")?;
    let record = Record {
        schema_version: 1,
        contract: "official_restart_http_v1",
        config_path: &outcome.config_path,
        confirmation,
    };
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, &record)?;
    std::io::Write::flush(&mut file)?;
    file.as_file().sync_all()?;
    Ok(PreparedOfficialConfirmation {
        file,
        root: root.to_path_buf(),
    })
}

/// Serialization and file fsync happen before the synchronous epoch fence.
/// Only the atomic namespace publication is serialized with Stop revocation.
pub(crate) struct PreparedOfficialConfirmation {
    file: tempfile::NamedTempFile,
    root: PathBuf,
}

impl PreparedOfficialConfirmation {
    pub(crate) fn publish(self) -> Result<()> {
        process_utils::atomic_file::persist(
            self.file,
            &self.root.join("official-proxy-confirmation.json"),
        )?;
        #[cfg(unix)]
        std::fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

pub fn confirmed_publication() -> Option<(CompileOutcome, super::apply_status::ConfirmedPublication)>
{
    CONFIRMED_PUBLICATION
        .lock()
        .ok()
        .and_then(|value| value.clone())
}

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

pub(crate) fn runtime_root(log_root: &Path) -> Result<PathBuf> {
    runtime_root_from_override(
        log_root,
        std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR").as_deref(),
    )
}

fn runtime_root_from_override(
    log_root: &Path,
    explicit: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let requested = match explicit {
        Some(path) => {
            anyhow::ensure!(
                !path.is_empty(),
                "APP_CLI_PINGAP_RUNTIME_DIR must not be empty"
            );
            PathBuf::from(path)
        }
        None => log_root.join("pingap"),
    };
    let absolute = std::path::absolute(&requested)
        .context("resolve proxy runtime directory against invocation directory")?;
    canonical_path_with_missing_tail(&absolute)
}

/// Resolve the existing ancestor through the filesystem before retaining a
/// missing suffix. In particular, `link/..` must follow the link before `..`.
pub(crate) fn canonical_path_with_missing_tail(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(ancestor).with_context(|| {
                    format!("resolve proxy runtime ancestor {}", ancestor.display())
                })?;
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("proxy runtime directory has no resolvable existing ancestor")?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .context("proxy runtime directory has no existing parent")?;
            }
            Err(error) => return Err(error).context("observe proxy runtime directory"),
        }
    }
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
    publish_to_path(candidate, &active).await?;
    Ok(active)
}

pub async fn publish_active_until(
    runtime_root: &Path,
    candidate: &Path,
    deadline: tokio::time::Instant,
) -> Result<PathBuf> {
    let active = active_config_path(runtime_root);
    let bytes = tokio::time::timeout_at(deadline, tokio::fs::read(candidate))
        .await
        .context("read candidate publication deadline exhausted")??;
    replace_config_bytes_until(&active, bytes, deadline).await?;
    Ok(active)
}

/// Atomic and durable same-directory replacement; no unlink window and no
/// shared temporary filename that another publisher can accidentally replace.
pub async fn publish_to_path(candidate: &Path, target: &Path) -> Result<()> {
    let bytes = tokio::fs::read(candidate)
        .await
        .context("read immutable proxy candidate")?;
    replace_config_bytes(target, bytes).await
}

static FILE_REPLACEMENTS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));

struct FilePersistenceStage {
    target: Option<PathBuf>,
    stage: &'static str,
    started: std::time::Instant,
    deadline: Option<tokio::time::Instant>,
    finished: bool,
}

impl FilePersistenceStage {
    fn start(
        target: Option<&Path>,
        stage: &'static str,
        deadline: Option<tokio::time::Instant>,
    ) -> Self {
        let trace = Self {
            target: target.map(Path::to_path_buf),
            stage,
            started: std::time::Instant::now(),
            deadline,
            finished: false,
        };
        trace.log("started", None);
        trace
    }

    fn log(&self, phase: &'static str, succeeded: Option<bool>) {
        macro_rules! stage_event {
            ($level:expr) => {
                tracing::event!(
                    $level,
                    target_path = self
                        .target
                        .as_deref()
                        .map(|target| tracing::field::display(target.display())),
                    persistence_stage = self.stage,
                    phase,
                    elapsed_ms = self.started.elapsed().as_millis() as u64,
                    remaining_budget_ms = self.deadline.map(|deadline| {
                        deadline
                            .saturating_duration_since(tokio::time::Instant::now())
                            .as_millis() as u64
                    }),
                    succeeded,
                    "file persistence stage"
                );
            };
        }
        if phase == "interrupted" || succeeded == Some(false) {
            stage_event!(tracing::Level::WARN);
        } else {
            stage_event!(tracing::Level::DEBUG);
        }
    }

    fn complete(mut self, succeeded: bool) {
        self.log("finished", Some(succeeded));
        self.finished = true;
    }

    fn finish<T, E>(self, result: std::result::Result<T, E>) -> std::result::Result<T, E> {
        self.complete(result.is_ok());
        result
    }
}

impl Drop for FilePersistenceStage {
    fn drop(&mut self) {
        if !self.finished {
            self.log("interrupted", None);
        }
    }
}

pub async fn wait_for_file_replacements() {
    let trace = FilePersistenceStage::start(None, "settle_lock_wait", None);
    let _guard = FILE_REPLACEMENTS.clone().lock_owned().await;
    trace.complete(true);
}

pub(crate) async fn wait_for_file_replacements_until(deadline: tokio::time::Instant) -> Result<()> {
    let trace = FilePersistenceStage::start(None, "settle_lock_wait", Some(deadline));
    let result = tokio::time::timeout_at(deadline, FILE_REPLACEMENTS.clone().lock_owned())
        .await
        .context("proxy configuration writes remain unsettled before shutdown deadline");
    let _guard = trace.finish(result)?;
    Ok(())
}

pub(crate) fn file_replacements_pending() -> bool {
    FILE_REPLACEMENTS.try_lock().is_err()
}

pub fn clear_current_confirmation() -> Result<()> {
    *CONFIRMED_PUBLICATION
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy publication lock poisoned"))? = None;
    let mut official = OFFICIAL_CONFIRMATION
        .lock()
        .map_err(|_| anyhow::anyhow!("official proxy confirmation lock poisoned"))?;
    if let Some(view) = official.as_mut() {
        view.confirmed = false;
        view.proxy_instance_id = None;
        if matches!(
            view.phase,
            shared_types::ProxyPhase::Serving | shared_types::ProxyPhase::Standby
        ) {
            view.phase = shared_types::ProxyPhase::Absent;
        }
    }
    record_expected_hash("");
    PUBLICATION_UNCERTAIN.store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

pub async fn replace_config_bytes(target: &Path, bytes: Vec<u8>) -> Result<()> {
    PUBLICATION_UNCERTAIN.store(true, std::sync::atomic::Ordering::Release);
    replace_file_bytes(target, bytes).await
}

pub async fn replace_config_bytes_until(
    target: &Path,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    replace_file_with_until(target.to_path_buf(), bytes, deadline, |target, bytes| {
        PUBLICATION_UNCERTAIN.store(true, std::sync::atomic::Ordering::Release);
        durable_replace(target, bytes)
    })
    .await
}

async fn replace_file_with_until<F>(
    target: PathBuf,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
    writer: F,
) -> Result<()>
where
    F: FnOnce(&Path, &[u8]) -> Result<()> + Send + 'static,
{
    let trace = FilePersistenceStage::start(Some(&target), "replace_file", Some(deadline));
    let result = async {
        let lock_trace = FilePersistenceStage::start(Some(&target), "lock_wait", Some(deadline));
        let result = tokio::time::timeout_at(deadline, FILE_REPLACEMENTS.clone().lock_owned())
            .await
            .context("file publication deadline exhausted before dispatch");
        let guard = lock_trace.finish(result)?;
        let queue_trace =
            FilePersistenceStage::start(Some(&target), "blocking_queue", Some(deadline));
        let task = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            queue_trace.complete(true);
            let writer_trace =
                FilePersistenceStage::start(Some(&target), "blocking_writer", Some(deadline));
            let result = (|| {
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "file publication deadline exhausted before dispatch"
                );
                writer(&target, &bytes)
            })();
            writer_trace.finish(result)
        });
        // Expiry ends only the caller's wait. The blocking writer keeps its lease
        // until real completion, so successors and owner retirement cannot race it.
        tokio::time::timeout_at(deadline, task)
            .await
            .context("proxy file publication did not settle before its deadline")?
            .context("proxy publication task failed")?
    }
    .await;
    trace.finish(result)
}

/// Generic metadata writes do not change the applied configuration state.
/// The blocking task retains its own serialization lease even when its caller
/// is cancelled. Successor writes and owner cleanup must await that same lease.
pub async fn replace_file_bytes(target: &Path, bytes: Vec<u8>) -> Result<()> {
    replace_file_with(target.to_path_buf(), bytes, durable_replace).await
}

async fn replace_file_with<F>(target: PathBuf, bytes: Vec<u8>, writer: F) -> Result<()>
where
    F: FnOnce(&Path, &[u8]) -> Result<()> + Send + 'static,
{
    let trace = FilePersistenceStage::start(Some(&target), "replace_file", None);
    let result = async {
        let lock_trace = FilePersistenceStage::start(Some(&target), "lock_wait", None);
        let guard = FILE_REPLACEMENTS.clone().lock_owned().await;
        lock_trace.complete(true);
        let queue_trace = FilePersistenceStage::start(Some(&target), "blocking_queue", None);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            queue_trace.complete(true);
            let writer_trace = FilePersistenceStage::start(Some(&target), "blocking_writer", None);
            writer_trace.finish(writer(&target, &bytes))
        })
        .await
        .context("durable file replacement task failed")?
    }
    .await;
    trace.finish(result)
}

fn durable_replace(target: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let trace = FilePersistenceStage::start(Some(target), "durable_replace", None);
    let result = (|| {
        let parent = target
            .parent()
            .context("file replacement target has no parent")?;
        let stage = FilePersistenceStage::start(Some(target), "create_directory", None);
        stage.finish(std::fs::create_dir_all(parent).context("create replacement directory"))?;
        let stage = FilePersistenceStage::start(Some(target), "allocate_temporary", None);
        let mut temporary = stage
            .finish(tempfile::NamedTempFile::new_in(parent).context("allocate file temporary"))?;
        let stage = FilePersistenceStage::start(Some(target), "file_write", None);
        stage.finish(temporary.write_all(bytes))?;
        let stage = FilePersistenceStage::start(Some(target), "file_sync_all", None);
        stage.finish(temporary.as_file().sync_all())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let stage = FilePersistenceStage::start(Some(target), "file_permissions", None);
            stage.finish(
                temporary
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600)),
            )?;
        }
        let stage = FilePersistenceStage::start(Some(target), "rename", None);
        stage.finish(
            process_utils::atomic_file::persist(temporary, target).context("publish durable file"),
        )?;
        #[cfg(unix)]
        {
            let stage = FilePersistenceStage::start(Some(target), "directory_sync_all", None);
            stage.finish(std::fs::File::open(parent).and_then(|directory| directory.sync_all()))?;
        }
        Ok(())
    })();
    trace.finish(result)
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
    // A live process cannot hot-reload its polling interval. Preserve all
    // bootstrap-only fields from the confirmed topology, including Custom's
    // explicit interval; only a genuinely new topology receives our default.
    if active.is_none() {
        cfg.basic.auto_restart_check_interval = Some(std::time::Duration::from_secs(2));
    }
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
    cfg.plugins.clear();
    cfg.locations.insert(
        "standby".into(),
        LocationConf {
            // 不写 path：默认权重 0 兜底，接管全部请求
            plugins: Some(vec!["standby".into()]),
            ..Default::default()
        },
    );
    // mock（Request 阶段直出，无 upstream）：status/headers/data。
    // `X-Pingap-EType: rcoder-standby` 证明响应为入口自产（该窗口无 upstream
    // 代理，应用不可能注入），rcoder-proxy 故障页管线据此替换为友好表示；
    // 业务状态细分（starting/stopped/failed）由就绪观察在错误时刻给出。
    // 正文用与 rcoder-proxy 同一套自包含错误页（含 5s meta refresh，无 JS
    // 依赖）：绕过 rcoder-proxy 替换层的路径（standby-origin 冷缓存首击透传、
    // 直连预览域名）首击即见可自动恢复的页面，而非裸文本。语言固定 zh-CN
    // （mock 是发布期静态产物，感知不了请求语言；代理路径由 rcoder-proxy 按
    // 请求语言覆盖为本地化版本，两者共用同一资产互不冲突）。
    // pingap 的 PluginConf 是自由键值袋（Map<String, Value>，模型层无插件
    // 类型）：字段名经 StandbyMockPlugin 在编译期钉住，序列化后落回 PluginConf。
    let body = standby_error_page(publication_id);
    let plugin: PluginConf = serde_json::from_value(serde_json::to_value(StandbyMockPlugin {
        category: "mock",
        status: 503,
        headers: vec![
            "Retry-After: 2".to_string(),
            "Content-Type: text/html; charset=utf-8".to_string(),
            "Cache-Control: no-store".to_string(),
            format!("X-Rcoder-Publication: {publication_id}"),
            format!("X-Pingap-EType: {}", shared_types::PINGAP_STANDBY_ETYPE),
        ],
        data: body,
    })?)?;
    cfg.plugins.insert("standby".into(), plugin);
    cfg = publication_config(cfg, publication_id, 503)?;
    cfg.validate().context("validate standby Pingap config")?;
    let expected_hash = cfg.hash().context("compute standby Pingap config hash")?;
    let content = toml::to_string_pretty(&cfg).context("serialize standby Pingap config")?;
    Ok((content, expected_hash))
}

/// standby 兜底页与 rcoder-proxy 的用户错误页共用同一资产；占位符按
/// restarting 档（"启动或重启"）渲染，诊断编号直接用发布 ID（可定位具体
/// 一次摘流发布）。UI 标签清单与 rcoder-proxy error_page::UI_LABELS 保持
/// 同步；`standby_page_renders_without_placeholders` 测试兜底防漂移。
const STANDBY_PAGE_TEMPLATE: &str =
    include_str!("../../../rcoder-proxy/assets/userapp-error-default.html");

const STANDBY_PAGE_LOCALE: &str = "zh-CN";

/// standby 兜底 mock 插件的字段契约（pingap PluginConf 是 Map<String, Value>
/// 自由键值袋，模型层无类型；本地结构体把字段名钉在编译期，键名拼写错误
/// 直接编译失败而不是静默产出不生效的 mock）。
#[derive(Debug, serde::Serialize)]
struct StandbyMockPlugin<'a> {
    category: &'a str,
    status: u16,
    headers: Vec<String>,
    data: String,
}

fn standby_error_page(publication_id: &str) -> String {
    let locale = STANDBY_PAGE_LOCALE;
    let slug = "restarting";
    let mut page = STANDBY_PAGE_TEMPLATE
        .replace(
            "{{RCODER_TITLE}}",
            &shared_types::t(&format!("userapp_error_page.{slug}.title"), locale),
        )
        .replace(
            "{{RCODER_MESSAGE}}",
            &shared_types::t(&format!("userapp_error_page.{slug}.message"), locale),
        )
        .replace("{{RCODER_DIAGNOSTIC_ID}}", publication_id)
        .replace("{{RCODER_STATUS}}", "503")
        .replace("{{RCODER_LANG}}", locale)
        .replace("{{RCODER_CAUSE}}", slug);
    const UI_LABELS: [(&str, &str); 16] = [
        ("{{RCODER_UI_BROWSER}}", "browser"),
        ("{{RCODER_UI_CONNECTED}}", "connected"),
        ("{{RCODER_UI_GATEWAY}}", "gateway"),
        ("{{RCODER_UI_NORMAL}}", "normal"),
        ("{{RCODER_UI_APP}}", "app_service"),
        ("{{RCODER_UI_UNAVAILABLE}}", "unavailable"),
        ("{{RCODER_UI_STARTING}}", "starting"),
        ("{{RCODER_UI_STOPPED}}", "stopped"),
        ("{{RCODER_UI_STOPPING}}", "stopping"),
        ("{{RCODER_UI_FAILED}}", "failed"),
        ("{{RCODER_UI_PREPARING}}", "preparing"),
        ("{{RCODER_UI_RELOAD}}", "reload"),
        ("{{RCODER_UI_HINT}}", "hint"),
        ("{{RCODER_UI_COPY}}", "copy"),
        ("{{RCODER_UI_COPIED}}", "copied"),
        ("{{RCODER_UI_ARIA}}", "aria_chain"),
    ];
    for (placeholder, key) in UI_LABELS {
        let text = shared_types::t(&format!("userapp_error_page.ui.{key}"), locale);
        page = page.replace(placeholder, &text);
    }
    page
}

#[cfg(test)]
mod standby_page_tests {
    use super::standby_error_page;

    /// 资产与 rcoder-proxy 共用：对方新增占位符而本清单未跟时，此处立即
    /// 失败（裸 `{{RCODER_*}}` 直出浏览器）。
    #[test]
    fn standby_page_renders_without_placeholders() {
        let page = standby_error_page("pub-123");
        assert!(
            page.contains("http-equiv=\"refresh\""),
            "missing meta refresh"
        );
        assert!(page.contains("pub-123"), "publication id not rendered");
        assert!(
            !page.contains("{{RCODER_"),
            "unresolved placeholder leaked into standby page"
        );
    }
}

/// 常驻热载兼容校验（P1 T15）：对比当前 active 与新候选的 server 拓扑。
///
/// pin（0.15.0）热载支持既有 server 的 locations/upstreams/plugins 更新；
/// **server 增删/改名/监听地址变化不可热载**——受理前 Fail Fast（保旧服务
/// 运行，指引走完整重配置流程），不得静默半生效。首编（active 不存在或
/// 无法解析）跳过校验（bootstrap 建立全量拓扑）。
pub fn validate_hot_reload_compatible(active: &Path, candidate: &str) -> Result<()> {
    let active_content = match std::fs::read_to_string(active) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read active config {} before preflight", active.display())
            });
        }
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
    if serde_json::to_value(&active_cfg.certificates)?
        != serde_json::to_value(&candidate_cfg.certificates)?
    {
        anyhow::bail!(
            "resident pingap hot-reload cannot apply certificate changes; use the full reconfiguration flow"
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
pub async fn publish_standby(runtime_root: &Path, publication_id: &str) -> Result<CompileOutcome> {
    publish_standby_to_path(
        runtime_root,
        &active_config_path(runtime_root),
        publication_id,
    )
    .await
}

pub async fn publish_standby_to_path(
    runtime_root: &Path,
    target: &Path,
    publication_id: &str,
) -> Result<CompileOutcome> {
    let content = match tokio::fs::read_to_string(target).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {} before proxy drain", target.display()));
        }
    };
    let (content, expected_hash) = build_standby_from_topology(publication_id, content.as_deref())?;
    let config =
        PingapConfig::new(content.as_bytes(), true).context("decode standby publication")?;
    let outcome = persist_candidate(runtime_root, &config, publication_id, 503).await?;
    anyhow::ensure!(
        outcome.expected_hash == expected_hash,
        "standby changed during serialization"
    );
    publish_to_path(&outcome.config_path, target).await?;
    Ok(outcome)
}

pub use super::publication::{
    publish_confirmed, publish_standby_confirmed, publish_standby_confirmed_to_path,
    publish_standby_confirmed_to_path_until,
};

pub async fn compile_and_validate(
    workspace: &Path,
    runtime_root: &Path,
    pingap_bin: &Path,
    release: &ReleaseLock,
    startup: StartupContext,
) -> Result<CompileOutcome> {
    compile_prepared(
        workspace,
        workspace,
        runtime_root,
        pingap_bin,
        release,
        startup,
    )
    .await
}

/// Freeze a completely validated publication before retiring the old execution.
/// Configuration is read from staging, while its file references point to the
/// execution directory that will exist after activation.
pub async fn compile_prepared(
    read_workspace: &Path,
    execution_workspace: &Path,
    runtime_root: &Path,
    pingap_bin: &Path,
    release: &ReleaseLock,
    startup: StartupContext,
) -> Result<CompileOutcome> {
    let layout = vec![runtime_root.to_path_buf()];
    let (content, _) =
        compile_effective_config_with_roots(read_workspace, release, &layout, startup.dev_profile)
            .await?;
    let mut config =
        PingapConfig::new(content.as_bytes(), true).context("decode prepared proxy graph")?;
    let publication = uuid::Uuid::new_v4().to_string();
    config = publication_config(config, &publication, 200)?;
    // Plugin constructors may read directory/file resources. Validate against
    // the immutable staged tree before activation, then freeze the exact graph
    // using its future execution paths. HTTP match paths are never relocated.
    let mut outcome = if read_workspace != execution_workspace {
        let validation_id = uuid::Uuid::new_v4().to_string();
        write_validated_candidate(runtime_root, pingap_bin, &config, &validation_id, 200).await?;
        rebind_file_references(&mut config, read_workspace, execution_workspace)?;
        persist_candidate(runtime_root, &config, &publication, 200).await?
    } else {
        write_validated_candidate(runtime_root, pingap_bin, &config, &publication, 200).await?
    };
    outcome.serving_release_id = Some(release.release_id.clone());
    outcome.business_probes = business_http_targets(release, startup);
    Ok(outcome)
}

fn business_http_targets(
    release: &ReleaseLock,
    startup: StartupContext,
) -> Vec<ServiceProbeTarget> {
    release
        .services
        .iter()
        .filter(|service| {
            service.enabled
                && service.kind == workspace_manifest::ProjectKind::Web
                && service.proxy.is_some()
                && matches!(
                    crate::startup_probe::effective(service, startup),
                    workspace_manifest::StartupProbe::Http
                )
        })
        .map(|service| ServiceProbeTarget {
            service_id: service.service_id.clone(),
            address: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, service.port)),
            path: if service.health.readiness_path.is_empty() {
                "/health".into()
            } else {
                service.health.readiness_path.clone()
            },
        })
        .collect()
}

fn rebind_file_references(
    config: &mut PingapConfig,
    source: &Path,
    execution: &Path,
) -> Result<()> {
    let source = std::fs::canonicalize(source).context("resolve prepared source directory")?;
    let relocate = |value: &mut String| {
        if let Ok(relative) = Path::new(value).strip_prefix(&source) {
            *value = execution.join(relative).to_string_lossy().into_owned();
        }
    };
    for plugin in config.plugins.values_mut() {
        let category = plugin
            .get("category")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_owned();
        for (key, value) in plugin.iter_mut() {
            if (key.contains("file")
                || key.contains("directory")
                || key.contains("cert")
                || (category == "directory" && key.contains("path")))
                && let toml::Value::String(path) = value
            {
                relocate(path);
            }
        }
    }
    for path in [
        &mut config.basic.pid_file,
        &mut config.basic.error_log,
        &mut config.basic.upgrade_sock,
    ]
    .into_iter()
    .flatten()
    {
        relocate(path);
    }
    Ok(())
}

pub(crate) async fn write_validated_candidate(
    runtime_root: &Path,
    pingap_bin: &Path,
    config: &PingapConfig,
    publication: &str,
    status: u16,
) -> Result<CompileOutcome> {
    validate_candidate(pingap_bin, config, runtime_root).await?;
    let outcome = persist_candidate(runtime_root, config, publication, status).await?;
    Ok(outcome)
}

/// Validate the complete candidate using the pinned official executable before
/// stopping an existing proxy or business process. No admin fallback is enabled
/// for `-t`: malformed configuration must remain a hard failure.
pub async fn validate_candidate(
    pingap_bin: &Path,
    config: &PingapConfig,
    runtime_root: &Path,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    validate_candidate_until(pingap_bin, config, runtime_root, deadline).await
}

/// Runtime transitions use their original deadline for capability observation,
/// native configuration validation, and cleanup. An earlier phase cannot grant
/// this preflight a fresh budget after the control request has waited.
pub(crate) async fn validate_candidate_until(
    pingap_bin: &Path,
    config: &PingapConfig,
    runtime_root: &Path,
    deadline: tokio::time::Instant,
) -> Result<()> {
    anyhow::ensure!(
        tokio::time::Instant::now() < deadline,
        "proxy candidate validation deadline exceeded before preflight"
    );
    let binary_digest = super::capabilities::preflight_until(pingap_bin, config, deadline).await?;
    tokio::time::timeout_at(deadline, tokio::fs::create_dir_all(runtime_root))
        .await
        .context("proxy validation root deadline exceeded")?
        .context("create proxy validation root")?;
    let directory = tempfile::Builder::new()
        .prefix("preflight-")
        .tempdir_in(runtime_root)
        .context("create private candidate validation directory")?;
    anyhow::ensure!(
        tokio::time::Instant::now() < deadline,
        "proxy candidate validation deadline exceeded before configuration write"
    );
    let target = directory.path().join("pingap.toml");
    tokio::time::timeout_at(
        deadline,
        tokio::fs::write(&target, toml::to_string_pretty(config)?),
    )
    .await
    .context("proxy validation configuration write deadline exceeded")?
    .context("write native proxy validation configuration")?;
    let mut command = Command::new(pingap_bin);
    super::official_env::sanitize(&mut command);
    command
        .arg("-t")
        .arg("-c")
        .arg(super::command_path::external_config_path(&target)?);
    let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
    anyhow::ensure!(
        !budget.is_zero(),
        "proxy native validation deadline exceeded"
    );
    let output = super::capabilities::native_test_until(command, deadline, directory)
        .await
        .with_context(|| format!("validate {} -t", pingap_bin.display()))?;
    super::capabilities::verify_binary_until(pingap_bin, &binary_digest, deadline).await?;
    anyhow::ensure!(
        output.status.success(),
        "pingap -t rejected candidate: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

pub(super) async fn persist_candidate(
    runtime_root: &Path,
    config: &PingapConfig,
    publication: &str,
    status: u16,
) -> Result<CompileOutcome> {
    let expected_hash = config.hash().context("prepared proxy hash")?;
    let config_digest = configuration_digest(config)?;
    let entry_probes = probe_targets(config, status)?;
    let target_dir = runtime_root.join("publications").join(publication);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .context("create immutable proxy publication directory")?;
    let target = target_dir.join("pingap.toml");
    let content = toml::to_string_pretty(config).context("serialize proxy publication")?;
    // A UUID directory is single-use. Never overwrite another admitted candidate.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .await
        .context("create proxy candidate")?;
    use tokio::io::AsyncWriteExt as _;
    file.write_all(content.as_bytes()).await?;
    file.sync_all().await?;
    set_private_permissions(&target).await?;
    Ok(CompileOutcome {
        serving_release_id: None,
        operation_id: None,
        config_path: target,
        expected_hash,
        publication_id: publication.into(),
        config_digest,
        entry_probes,
        business_probes: Vec::new(),
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
        PUBLICATION_OBJECT, active_config_path, build_standby_config, managed_config,
        publish_active, publish_standby, validate_hot_reload_compatible, validate_plugin_paths,
        validate_upstream_destination,
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
        let (content, hash) =
            build_standby_config("11111111-1111-4111-8111-111111111111").expect("standby config");
        assert!(!hash.is_empty());
        let cfg: pingap_config::PingapConfig =
            toml::from_str(&content).expect("standby config parses");
        let server = cfg.servers.get("app").expect("server app");
        assert_eq!(server.addr, format!("0.0.0.0:{PINGAP_PORT}"));
        assert_eq!(
            server.locations.as_deref(),
            Some(&[PUBLICATION_OBJECT.to_string(), "standby".to_string()][..]),
            "publication marker plus catch-all standby cover all paths"
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
        assert!(
            joined.contains(&format!(
                "X-Pingap-EType: {}",
                shared_types::PINGAP_STANDBY_ETYPE
            )),
            "standby origin marker required for error-page replacement: {joined}"
        );
        // pin（0.15.0 文件模式）周期轮询默认 90s——必须收紧到确认预算内
        assert_eq!(
            cfg.basic.auto_restart_check_interval,
            Some(std::time::Duration::from_secs(2))
        );
    }

    #[test]
    fn standby_preserves_custom_bootstrap_settings_and_every_listener() {
        let mut active = pingap_config::PingapConfig::default();
        active.basic.auto_restart_check_interval = Some(std::time::Duration::from_secs(5));
        active.basic.threads = Some(3);
        for (name, address) in [("public", "0.0.0.0:9080"), ("internal", "127.0.0.1:19080")] {
            active.servers.insert(
                name.into(),
                pingap_config::ServerConf {
                    addr: address.into(),
                    locations: Some(vec!["old".into()]),
                    ..Default::default()
                },
            );
        }
        let original = toml::to_string_pretty(&active).unwrap();
        let (standby, _) =
            super::build_standby_from_topology(&uuid::Uuid::new_v4().to_string(), Some(&original))
                .unwrap();
        let standby = pingap_config::PingapConfig::new(standby.as_bytes(), true).unwrap();
        assert_eq!(
            serde_json::to_value(&active.basic).unwrap(),
            serde_json::to_value(&standby.basic).unwrap()
        );
        assert_eq!(standby.servers.len(), 2);
        for (name, old) in &active.servers {
            let new = &standby.servers[name];
            assert_eq!(new.addr, old.addr);
            assert!(new.locations.as_ref().unwrap().contains(&"standby".into()));
        }
    }

    #[test]
    fn unreadable_active_configuration_is_not_treated_as_first_bootstrap() {
        let root = tempfile::tempdir().unwrap();
        let active_directory = root.path().join("pingap.toml");
        std::fs::create_dir(&active_directory).unwrap();
        let candidate = "[servers.app]\naddr='0.0.0.0:9080'\n";
        let error =
            super::validate_hot_reload_compatible(&active_directory, candidate).unwrap_err();
        assert!(format!("{error:#}").contains("before preflight"));
        assert!(active_directory.is_dir());
    }

    #[tokio::test]
    async fn cancelled_atomic_writer_cannot_overtake_successor_or_owner_cleanup() {
        struct Release(std::sync::mpsc::Sender<()>);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let root = tempfile::tempdir().unwrap();
        let active = root.path().join("active.toml");
        std::fs::write(&active, b"original").unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let release = Release(release);
        let target = active.clone();
        let first = tokio::spawn(super::replace_file_with(
            target,
            b"old-deferred".to_vec(),
            move |target, bytes| {
                let _ = entered.send(());
                blocked.recv().unwrap();
                super::durable_replace(target, bytes)
            },
        ));
        ready.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let target = active.clone();
        let next =
            tokio::spawn(
                async move { super::replace_file_bytes(&target, b"latest".to_vec()).await },
            );
        let cleanup = tokio::spawn(super::wait_for_file_replacements());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !next.is_finished(),
            "successor must wait for a dispatched rename"
        );
        assert!(
            !cleanup.is_finished(),
            "owner cleanup must not release before pending writes finish"
        );
        drop(release);
        next.await.unwrap().unwrap();
        cleanup.await.unwrap();
        assert_eq!(std::fs::read(active).unwrap(), b"latest");
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
        let hash = publish_standby(&runtime_root, "11111111-1111-4111-8111-111111111111")
            .await
            .unwrap();
        assert!(!hash.expected_hash.is_empty());
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

#[cfg(test)]
#[path = "compiler_startup_tests.rs"]
mod startup_contract_tests;

#[cfg(test)]
#[path = "compiler_deadline_tests.rs"]
mod file_deadline_tests;

#[cfg(test)]
mod runtime_root_tests {
    use super::runtime_root_from_override;
    use std::{ffi::OsStr, path::Path};

    #[test]
    fn default_root_retains_missing_log_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let log = temporary.path().join("future-logs");
        let resolved = runtime_root_from_override(&log, None).unwrap();
        assert_eq!(
            resolved,
            temporary
                .path()
                .canonicalize()
                .unwrap()
                .join("future-logs/pingap")
        );
        assert!(
            !log.exists(),
            "resolving a root must not create directories"
        );
    }

    #[test]
    fn relative_override_uses_invocation_directory_not_log_root() {
        let name = format!("proxy-runtime-{}", uuid::Uuid::new_v4());
        let expected = std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(&name);
        let resolved =
            runtime_root_from_override(Path::new("unrelated-logs"), Some(OsStr::new(&name)))
                .unwrap();
        assert_eq!(resolved, expected);
        assert!(!resolved.exists());
    }

    #[test]
    fn empty_override_is_an_error_without_default_fallback() {
        let error =
            runtime_root_from_override(Path::new("logs"), Some(OsStr::new(""))).unwrap_err();
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn normal_existing_parent_traversal_resolves_before_missing_tail() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("existing")).unwrap();
        let requested = temporary.path().join("existing/../future/runtime");
        assert_eq!(
            runtime_root_from_override(Path::new("unused"), Some(requested.as_os_str())).unwrap(),
            temporary
                .path()
                .canonicalize()
                .unwrap()
                .join("future/runtime")
        );
    }

    #[test]
    fn non_directory_ancestor_error_is_not_replaced_by_another_root() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        let requested = file.join("runtime");
        assert!(runtime_root_from_override(temporary.path(), Some(requested.as_os_str())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_parent_traversal_uses_physical_target() {
        let temporary = tempfile::tempdir().unwrap();
        let physical = temporary.path().join("physical");
        std::fs::create_dir_all(physical.join("nested")).unwrap();
        std::os::unix::fs::symlink(physical.join("nested"), temporary.path().join("alias"))
            .unwrap();
        let requested = temporary.path().join("alias/../future/runtime");
        assert_eq!(
            runtime_root_from_override(Path::new("unused"), Some(requested.as_os_str())).unwrap(),
            physical.canonicalize().unwrap().join("future/runtime")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_is_an_observation_error_not_a_missing_suffix() {
        let temporary = tempfile::tempdir().unwrap();
        let link = temporary.path().join("broken");
        std::os::unix::fs::symlink(temporary.path().join("missing"), &link).unwrap();
        let requested = link.join("runtime");
        assert!(runtime_root_from_override(temporary.path(), Some(requested.as_os_str())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_override_preserves_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let temporary = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"runtime-\xff".to_vec());
        let requested = temporary.path().join(&name);
        assert_eq!(
            runtime_root_from_override(Path::new("unused"), Some(requested.as_os_str())).unwrap(),
            temporary.path().canonicalize().unwrap().join(name)
        );
    }
}

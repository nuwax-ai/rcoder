use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    Run,
    /// Recreate the management plane, with automatic business startup suppressed.
    Stopped,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Generation {
    pub version: u32,
    pub id: String,
    pub supervisor: String,
    pub token: String,
    pub intent: Intent,
    pub phase: GenerationPhase,
    /// Launch handshake and read-only exit inspection in the verified process
    /// space. Signals always use the retained Child, never this recorded PID.
    #[serde(default)]
    pub worker_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_domain: Option<crate::domain::PhysicalDomain>,
    /// Identity of the OS boot / PID namespace that ran this generation.
    /// A changed epoch is positive local proof the generation's processes
    /// cannot run again; records without it stay conservative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_epoch: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GenerationPhase {
    Pending,
    Running,
    Draining,
    Quiescent,
    Revoked,
}

pub(crate) fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("receipt parent missing")?;
    process_utils::command_context::create_durable_directory(parent)?;
    // Serialize before file I/O: to_writer on an unbuffered File issues a write
    // for every JSON fragment, delaying the control loop as replay history grows.
    let bytes = serde_json::to_vec(value)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, path)
        .with_context(|| format!("publish receipt {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read receipt {}", path.display()))?,
    )
    .with_context(|| format!("decode receipt {}", path.display()))
}
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().context("execution scope is still owned")?;
    Ok(file)
}
pub(crate) fn is_locked(path: &Path) -> Result<()> {
    let file = File::options().read(true).write(true).open(path)?;
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(()) => anyhow::bail!("execution owner has exited"),
    }
}
pub(crate) fn generation(root: &Path) -> Result<Generation> {
    let value: Generation = read(&root.join("generation.json"))?;
    ensure!(
        value.version == 1 && root.file_name().and_then(|s| s.to_str()) == Some(&value.id),
        "generation identity/version mismatch"
    );
    Ok(value)
}

/// RV06：worker 进程创建身份（additive sidecar，旧二进制忽略）。Windows
/// 无内核 boot UUID，uptime 单调只证明"未回退"——磁盘记录的 worker PID
/// 被观察为存活时，用创建时间核对是否仍是原进程：不一致 = PID 已被
/// 无关进程复用，原 worker 视为退出（不再阻塞清理）。
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerCreated {
    pub version: u32,
    /// worker（统一 owner 自身/旧 worker 进程）的创建时刻，unix 毫秒。
    pub unix_ms: u64,
}

#[cfg(windows)]
pub(crate) fn save_worker_created(root: &Path, value: &WorkerCreated) -> Result<()> {
    save(&root.join("worker-created.json"), &value)
}

/// 读取 worker 创建身份；缺失/损坏返回 None（legacy 记录走保守路径，
/// 不因诊断 sidecar 拒绝恢复）。
#[cfg(windows)]
pub(crate) fn worker_created(root: &Path) -> Option<u64> {
    let bytes = std::fs::read(root.join("worker-created.json")).ok()?;
    let value: WorkerCreated = serde_json::from_slice(&bytes).ok()?;
    (value.version == 1).then_some(value.unix_ms)
}
pub(crate) fn work_root(scope: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "invalid execution generation"
    );
    Ok(scope.join("work").join(id))
}

/// An identity-bound local cleanup receipt, not business success evidence.
#[derive(Clone, Debug)]
pub struct Quiescence {
    pub generation: String,
    pub supervisor_id: String,
}

pub fn verify_quiescent(scope: &Path, id: &str) -> Result<Quiescence> {
    let root = work_root(scope, id)?;
    let _lock = lock(&root.join("generation.lock"))?;
    let record = generation(&root)?;
    ensure!(
        matches!(
            record.phase,
            GenerationPhase::Quiescent | GenerationPhase::Revoked
        ),
        "execution generation has no cleanup receipt"
    );
    if record.phase == GenerationPhase::Revoked {
        process_utils::guardian::recover(&root)?;
        process_utils::command_context::require_quiescent(&root.join("commands"))?;
    }
    // Quiescent is the aggregate receipt written only after the worker,
    // command trees and external engine finished cleanup. Historical command
    // diagnostics cannot invalidate that completed cleanup later.
    Ok(Quiescence {
        generation: record.id,
        supervisor_id: record.supervisor,
    })
}

/// Confirm cleanup in this process domain. `None` means the record belongs to
/// a previous container on the same managed workspace, not that it exited.
/// The platform owns cross-container retirement; a local Stop cannot stop or
/// wait for processes in that other container.
pub fn verify_local_quiescent(scope: &Path, id: &str) -> Result<Option<Quiescence>> {
    if belongs_to_previous_container(scope, id)? {
        return Ok(None);
    }
    verify_quiescent(scope, id).map(Some)
}

pub(crate) fn belongs_to_previous_container(scope: &Path, id: &str) -> Result<bool> {
    let value = generation(&work_root(scope, id)?)?;
    Ok(previous_container(
        &value,
        crate::domain::PhysicalDomain::from_env()?.as_ref(),
    ))
}

/// Launch-time variant of [`belongs_to_previous_container`] with the process
/// epoch fallback (see [`previous_container_for_launch`]): monitor's
/// first-launch classification uses it so unstamped legacy records from a
/// replaced container stop forcing recovery semantics.
pub(crate) fn belongs_to_previous_container_for_launch(
    scope: &Path,
    id: &str,
    current: Option<&crate::domain::PhysicalDomain>,
) -> Result<bool> {
    let value = generation(&work_root(scope, id)?)?;
    Ok(previous_container_for_launch(&value, current))
}

fn previous_container(value: &Generation, current: Option<&crate::domain::PhysicalDomain>) -> bool {
    matches!((value.physical_domain.as_ref(), current), (Some(old), Some(current))
        if old.authority == current.authority
            && old.volume == current.volume
            && old.instance != current.instance)
}

/// 启动期跨容器分类（monitor 首启判定专用）：在 [`previous_container`] 的
/// 域印章判定之上，对无印章记录或同一物理域以进程纪元兜底——K8s 上早于
/// RCODER_EXECUTION_DOMAIN 注入的存量 Deployment 写下的记录都带 pid1 纪元，
/// 纪元不同即证明记录属于另一容器的进程空间。缺此兜底时无印章记录一律
/// 保守判为本容器残留 → 恢复语义剥掉一次性部署声明 → K8s 冷部署稳态卡死。
/// 同一 Pod UID 下重启容器也会更换 pid1 纪元；不同 authority/volume 的印章
/// 则不能被本地纪元覆盖。
///
/// 退役路径（reconcile/verify_local_quiescent）必须继续用纯印章的
/// [`previous_container`]：无印章即 native 场景，epoch 本身就是身份、允许
/// 本地退役；盖章的外容器历史归平台退役，本地 epoch 永远不得染指。
fn previous_container_for_launch(
    value: &Generation,
    current: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    if previous_container(value, current) {
        return true;
    }
    // An epoch is local process-space evidence, never authority over another
    // stamped volume/cluster. It also covers a restart inside the SAME Pod UID.
    if !process_space_ended_with(value, current) {
        return false;
    }
    let (Some(recorded), Some(now)) = (value.process_epoch.as_deref(), crate::epoch::current())
    else {
        return false;
    };
    crate::epoch::proves_replacement(recorded, &now)
}

/// Whether any recorded worker generation ran in the current container.
/// Generations stamped by a previous container are excluded by domain stamp
/// or, for unstamped legacy records, by process epoch; a record with neither
/// evidence (or unreadable evidence) counts as current, so uncertainty keeps
/// recovery semantics instead of authorizing a fresh launch.
pub(crate) fn current_container_work_exists_with(
    scope: &Path,
    current: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    work_history_for_launch(scope, current) == LaunchHistory::CurrentOrUnknown
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchHistory {
    Empty,
    PreviousContainerOnly,
    CurrentOrUnknown,
}

/// Inspect all receipts: discovery can lag generation creation after a crash.
/// Empty history is not proof that a pending control belongs to an old container.
pub(crate) fn work_history_for_launch(
    scope: &Path,
    current: Option<&crate::domain::PhysicalDomain>,
) -> LaunchHistory {
    let entries = match std::fs::read_dir(scope.join("work")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LaunchHistory::Empty,
        Err(error) => {
            tracing::warn!(%error, "work directory unreadable; assuming local work exists");
            return LaunchHistory::CurrentOrUnknown;
        }
    };
    let mut history = LaunchHistory::Empty;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "work entry unreadable; assuming local work exists");
                return LaunchHistory::CurrentOrUnknown;
            }
        };
        let root = entry.path();
        match root.join("generation.json").try_exists() {
            Ok(false) => continue, // legacy adapter owns legacy receipts
            Ok(true) => {}
            Err(error) => {
                tracing::warn!(%error, "generation path unreadable; assuming local work exists");
                return LaunchHistory::CurrentOrUnknown;
            }
        }
        match generation(&root) {
            Ok(value) => {
                if !previous_container_for_launch(&value, current) {
                    return LaunchHistory::CurrentOrUnknown;
                }
                history = LaunchHistory::PreviousContainerOnly;
            }
            Err(error) => {
                tracing::warn!(%error, "unreadable generation record; assuming local work exists");
                return LaunchHistory::CurrentOrUnknown;
            }
        }
    }
    history
}

pub fn verify_live(scope: &Path, supervisor: &str, id: &str) -> Result<()> {
    let root = work_root(scope, id)?;
    let generation = generation(&root)?;
    ensure!(
        generation.supervisor == supervisor && generation.phase == GenerationPhase::Running,
        "execution identity is not active"
    );
    is_locked(&root.join("generation.lock"))?;
    is_locked(&scope.join("owner.lock"))?;
    Ok(())
}

/// Run only with the stable owner lock held. A late root guardian must obtain
/// this same generation lock and cannot consume revoked authorization.
#[derive(Debug)]
pub(crate) struct AbandonedGeneration {
    pub root: PathBuf,
    pub value: Generation,
    // Retained through asynchronous cleanup: no late guardian can spawn work.
    pub _lock: File,
}

pub(crate) fn reconcile(scope: &Path) -> Result<Option<AbandonedGeneration>> {
    reconcile_local(scope, crate::domain::PhysicalDomain::from_env()?.as_ref())
}

/// Unified-owner variant（recovery v2 R2）：skip generations whose supervisor
/// is the current session instance. Those belong to THIS owner's in-process
/// business lifecycle（driver end / background cleanup already tracked in
/// memory）; reconciling them as "abandoned" would test the live owner's own
/// PID for exit and wedge recovery after a transient engine failure.
pub(crate) fn reconcile_foreign(
    scope: &Path,
    instance: &str,
) -> Result<Option<AbandonedGeneration>> {
    let current = crate::domain::PhysicalDomain::from_env()?;
    let dir = scope.join("work");
    if !dir.try_exists()? {
        return Ok(None);
    }
    let mut abandoned = None;
    for entry in std::fs::read_dir(dir)? {
        let root = entry?.path();
        if !root.join("generation.json").try_exists()? {
            continue;
        }
        let value = generation(&root)?;
        if value.supervisor == instance {
            // Own in-process generation: tracked by the session itself.
            continue;
        }
        // Mirror reconcile_local's retirement/classification for the rest.
        if previous_container(&value, current.as_ref()) {
            continue;
        }
        let generation_lock = lock(&root.join("generation.lock"))?;
        let mut value = generation(&root)?;
        let retirable = matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        );
        if retirable
            && (crate::domain::has_confirmed_exit(&root, &value)?
                || local_process_space_ended(&value)?)
        {
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            process_utils::guardian::confirm_physical_domain_exit(&root)?;
            retire_confirmed(&mut value);
            save(&root.join("generation.json"), &value)?;
        }
        if value.phase == GenerationPhase::Pending {
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            value.phase = GenerationPhase::Revoked;
            save(&root.join("generation.json"), &value)?;
        }
        if matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        ) {
            verify_abandoned_worker(&root, &value, current.as_ref())?;
            if abandoned.is_none() {
                abandoned = Some(AbandonedGeneration {
                    root,
                    value,
                    _lock: generation_lock,
                });
            }
            continue;
        }
        ensure!(
            matches!(
                value.phase,
                GenerationPhase::Quiescent | GenerationPhase::Revoked
            ),
            "generation {} cleanup is unconfirmed: {:?}",
            value.id,
            value.phase
        );
        if value.phase == GenerationPhase::Revoked {
            process_utils::guardian::recover(&root)?;
            process_utils::command_context::require_quiescent(&root.join("commands"))?;
        }
    }
    Ok(abandoned)
}

fn reconcile_local(
    scope: &Path,
    current: Option<&crate::domain::PhysicalDomain>,
) -> Result<Option<AbandonedGeneration>> {
    let dir = scope.join("work");
    if !dir.try_exists()? {
        return Ok(None);
    }
    let mut abandoned = None;
    for entry in std::fs::read_dir(dir)? {
        let root = entry?.path();
        if !root.join("generation.json").try_exists()? {
            continue;
        } // legacy adapter owns legacy receipts
        let value = generation(&root)?;
        if previous_container(&value, current) {
            // Keep foreign process and command receipts unchanged. A replacement
            // container must not interpret them as live work in its own process
            // namespace, or depend on a deleted Pod to bootstrap management.
            tracing::debug!(generation = %value.id,
                "previous container execution retained as history; recovering local management");
            continue;
        }
        let generation_lock = lock(&root.join("generation.lock"))?;
        let mut value = generation(&root)?;
        let retirable = matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        );
        if retirable
            && (crate::domain::has_confirmed_exit(&root, &value)?
                || local_process_space_ended(&value)?)
        {
            // Platform receipt or process-space epoch both prove the physical
            // runtime ended; neither invents an exit code nor touches journals.
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            process_utils::guardian::confirm_physical_domain_exit(&root)?;
            retire_confirmed(&mut value);
            save(&root.join("generation.json"), &value)?;
        }
        if value.phase == GenerationPhase::Pending {
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            value.phase = GenerationPhase::Revoked;
            save(&root.join("generation.json"), &value)?;
        }
        if matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        ) {
            verify_abandoned_worker(&root, &value, current)?;
            // The caller must perform full command/engine cleanup before it
            // may publish a terminal receipt or launch a successor.
            if abandoned.is_none() {
                abandoned = Some(AbandonedGeneration {
                    root,
                    value,
                    _lock: generation_lock,
                });
            }
            // Inspect all local generations before any external engine write.
            // A newer live worker must not be stopped by an older receipt's
            // cleanup, regardless of read_dir ordering.
            continue;
        }
        ensure!(
            matches!(
                value.phase,
                GenerationPhase::Quiescent | GenerationPhase::Revoked
            ),
            "generation {} cleanup is unconfirmed: {:?}",
            value.id,
            value.phase
        );
        if value.phase == GenerationPhase::Revoked {
            process_utils::guardian::recover(&root)?;
            process_utils::command_context::require_quiescent(&root.join("commands"))?;
        }
    }
    Ok(abandoned)
}

fn verify_abandoned_worker(
    #[cfg_attr(unix, allow(unused_variables))] root: &Path,
    value: &Generation,
    current: Option<&crate::domain::PhysicalDomain>,
) -> Result<()> {
    ensure!(
        process_space_ended_with(value, current),
        "generation {} belongs to another physical domain",
        value.id
    );
    let recorded = value
        .process_epoch
        .as_deref()
        .context("abandoned worker process epoch missing")?;
    let now = crate::epoch::current().context("current process epoch unavailable")?;
    ensure!(
        crate::epoch::same_process_space(recorded, &now),
        "generation {} process space cannot be confirmed",
        value.id
    );
    let pid = value
        .worker_pid
        .context("abandoned worker PID missing; cleanup cannot be confirmed")?;
    #[cfg(unix)]
    {
        ensure!(
            !process_utils::process_exists(pid).context("inspect original worker process")?,
            "generation {} worker {pid} still exists; cleanup is unconfirmed",
            value.id
        );
        ensure!(
            crate::epoch::current().as_deref() == Some(now.as_str()),
            "process space changed during worker inspection"
        );
        Ok(())
    }
    #[cfg(windows)]
    {
        // R6：只读 OpenProcess+signaled 观察陈旧 PID（终止的进程即使
        // 句柄被保留也观察为不存在）。观察后复核 uptime 仍单调，排除
        // 窗口内重启。
        if process_utils::process_exists(pid).context("inspect original worker process")? {
            // RV06：观察阳性时核对进程创建身份——原 worker 的创建时刻
            // 已随代次记录（worker-created.json sidecar）；PID 复用为无关
            // 进程（创建时刻不一致）不阻塞清理，原 worker 视为退出。
            // legacy 记录或身份不可读保持保守（可查询原因，非永久拒绝）。
            let identity = worker_created(root)
                .or_else(|| {
                    tracing::warn!(
                        generation = %value.id,
                        "worker creation identity missing; PID-reuse cannot be excluded"
                    );
                    None
                })
                .and_then(|recorded_ms| {
                    process_utils::process_created_unix_ms(pid)
                        .context("inspect worker process creation time")
                        .map(|observed| (recorded_ms, observed))
                        .ok()
                });
            match identity {
                Some((recorded_ms, Some(observed))) if observed.abs_diff(recorded_ms) > 1_000 => {
                    tracing::warn!(
                        generation = %value.id,
                        pid,
                        "recorded worker PID hosts an unrelated process; original worker is gone"
                    );
                }
                Some((_, Some(_))) => {
                    anyhow::bail!(
                        "generation {} worker {pid} still exists (creation identity matches); \
                         cleanup is unconfirmed",
                        value.id
                    );
                }
                _ => {
                    anyhow::bail!(
                        "generation {} worker {pid} observed alive; creation identity \
                         unavailable, cleanup stays unconfirmed",
                        value.id
                    );
                }
            }
        }
        let after = crate::epoch::current().context("re-read process epoch")?;
        ensure!(
            crate::epoch::same_process_space(&now, &after),
            "process space changed during worker inspection"
        );
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("local worker {pid} exit inspection is unsupported on this platform")
}

/// Terminal retirement of one generation. No exit code is invented and no
/// business journal is edited; cleanup is process evidence, not success.
fn retire_confirmed(value: &mut Generation) {
    value.phase = GenerationPhase::Quiescent;
    value.error =
        Some("physical runtime confirmed exit; business outcomes remain unchanged".into());
}

/// The generation's process space (OS boot or PID namespace) was replaced
/// after it stopped writing, so none of its processes can still exist. This
/// only speaks for generations that ran in OUR process space: a generation
/// stamped with a different physical domain (another container/pod) must be
/// retired by platform evidence, never by the local epoch.
fn local_process_space_ended(value: &Generation) -> Result<bool> {
    let (Some(recorded), Some(current)) = (value.process_epoch.as_deref(), crate::epoch::current())
    else {
        return Ok(false);
    };
    if !crate::epoch::proves_replacement(recorded, &current) {
        return Ok(false);
    }
    Ok(process_space_ended_with(
        value,
        crate::domain::PhysicalDomain::from_env()?.as_ref(),
    ))
}

/// Epoch-side scoping only: a changed epoch proves the end of the process
/// space that ran this generation, but a stamped generation belongs to one
/// container/pod, so the local proof may only fire when that identity is the
/// current environment's own domain.
fn process_space_ended_with(
    value: &Generation,
    current_domain: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    match value.physical_domain.as_ref() {
        // Only native (un-stamped) scopes may use the bare local proof.
        None => true,
        Some(domain) => current_domain == Some(domain),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::EpochGuard;

    /// Minimal stuck-generation scope: Running, no cleanup receipts, a live
    /// command record and an initialized admission gate.
    fn stuck_scope(
        domain: Option<crate::domain::PhysicalDomain>,
        epoch: Option<String>,
    ) -> (tempfile::TempDir, String) {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let root = work_root(temp.path(), &id).unwrap();
        std::fs::create_dir_all(root.join("commands")).unwrap();
        process_utils::command_authority::Gate::try_acquire(&root)
            .unwrap()
            .initialize()
            .unwrap();
        save(
            &root.join("generation.json"),
            &Generation {
                version: 1,
                id: id.clone(),
                supervisor: "gone-supervisor".into(),
                token: "test".into(),
                intent: Intent::Run,
                phase: GenerationPhase::Running,
                worker_pid: Some(4242),
                exit_code: None,
                error: None,
                physical_domain: domain,
                process_epoch: epoch,
            },
        )
        .unwrap();
        save(
            &root.join("commands/command.json"),
            &serde_json::json!({"version":1,"phase":"Running","identity":{"task_id":"original"}}),
        )
        .unwrap();
        (temp, id)
    }

    fn domain_fixture(instance: &str) -> crate::domain::PhysicalDomain {
        crate::domain::PhysicalDomain {
            authority: "k8s".into(),
            instance_source_env: None,
            instance: instance.into(),
            volume: "workspace-pvc".into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_worker_requires_dead_pid_in_same_known_process_space() {
        let epoch = "pid1:11111111-1111-4111-8111-111111111111:100".to_string();
        let _guard = EpochGuard::new(Some(epoch.clone()));
        let (temp, id) = stuck_scope(None, Some(epoch));
        let root = work_root(temp.path(), &id).unwrap();
        let mut value = generation(&root).unwrap();
        value.worker_pid = None;
        assert!(verify_abandoned_worker(&root, &value, None).is_err());
        value.worker_pid = Some(std::process::id());
        assert!(verify_abandoned_worker(&root, &value, None).is_err());
        let mut child = std::process::Command::new("true").spawn().unwrap();
        value.worker_pid = Some(child.id());
        child.wait().unwrap();
        for phase in [GenerationPhase::Running, GenerationPhase::Draining] {
            value.phase = phase;
            save(&root.join("generation.json"), &value).unwrap();
            let permit = reconcile_local(temp.path(), None).unwrap().unwrap();
            assert_eq!(permit.value.id, id);
            assert!(lock(&root.join("generation.lock")).is_err());
            // Discovering a candidate is not cleanup or business success.
            assert_eq!(generation(&root).unwrap().phase, phase);
            drop(permit);
        }
        value.process_epoch = Some("unknown".into());
        assert!(verify_abandoned_worker(&root, &value, None).is_err());
        value.process_epoch = Some("pid1:11111111-1111-4111-8111-111111111111:100".into());
        value.physical_domain = Some(domain_fixture("pod-original"));
        assert!(verify_abandoned_worker(&root, &value, Some(&domain_fixture("pod-new"))).is_err());
        assert!(
            verify_abandoned_worker(&root, &value, Some(&domain_fixture("pod-original"))).is_ok()
        );
        let mut live = value.clone();
        live.id = uuid::Uuid::new_v4().to_string();
        live.physical_domain = None;
        live.worker_pid = Some(std::process::id());
        let live_root = work_root(temp.path(), &live.id).unwrap();
        std::fs::create_dir_all(&live_root).unwrap();
        save(&live_root.join("generation.json"), &live).unwrap();
        assert!(
            reconcile_local(temp.path(), None).is_err(),
            "old cleanup must not affect a newer live generation"
        );
    }

    #[test]
    fn previous_container_records_do_not_count_as_local_work() {
        let (temp, _id) = stuck_scope(Some(domain_fixture("pod-old")), None);
        assert!(!current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
    }

    #[test]
    fn current_container_records_count_as_local_work() {
        let (temp, _id) = stuck_scope(Some(domain_fixture("pod-same")), None);
        assert!(current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-same"))
        ));
    }

    #[test]
    fn missing_domain_stamp_counts_as_local_work() {
        // 无法盖章的环境保持保守：记录一律视为本容器工作，不得触发全新启动。
        let (temp, _id) = stuck_scope(None, None);
        assert!(current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
        assert!(current_container_work_exists_with(temp.path(), None));
    }

    /// 存量无印章记录（旧 app Deployment 未注入执行域 env 写下的）以进程
    /// 纪元兜底：记录的 pid1 与当前纪元不同 → 前容器残留 → 不再被恢复
    /// 语义吞掉一次性部署声明（K8s 冷部署稳态卡死的根因回归锁）。
    #[test]
    fn unstamped_record_with_foreign_epoch_is_previous_container() {
        let (temp, _id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        assert!(!current_container_work_exists_with(temp.path(), None));
        assert!(!current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
    }

    /// 同容器残留（owner 崩溃重启、pid1 未变）：纪元相同不构成跨容器证据，
    /// 恢复语义保持——不重放一次性部署输入。
    #[test]
    fn unstamped_record_with_current_epoch_stays_local_work() {
        let (temp, _id) = stuck_scope(
            None,
            Some("pid1:22222222-2222-4222-8222-222222222222:100".into()),
        );
        let _guard = EpochGuard::new(Some("pid1:22222222-2222-4222-8222-222222222222:100".into()));
        assert!(current_container_work_exists_with(temp.path(), None));
        assert!(current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
    }

    #[test]
    fn launch_epoch_does_not_override_conflicting_domain() {
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        let (temp, _) = stuck_scope(
            Some(domain_fixture("pod-same")),
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        for current in [
            None,
            Some(crate::domain::PhysicalDomain {
                authority: "another-cluster".into(),
                ..domain_fixture("pod-same")
            }),
            Some(crate::domain::PhysicalDomain {
                volume: "another-volume".into(),
                ..domain_fixture("pod-same")
            }),
        ] {
            assert!(
                current_container_work_exists_with(temp.path(), current.as_ref()),
                "an unrelated local epoch cannot override a stamped domain: {current:?}"
            );
        }
    }

    #[test]
    fn epoch_guard_restores_platform_reader() {
        {
            let _guard = EpochGuard::new(None);
            assert!(crate::epoch::current().is_none());
        }
        assert!(crate::epoch::current().is_some());
    }

    #[test]
    fn no_work_directory_is_not_local_work() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
    }

    #[test]
    fn unreadable_generation_counts_as_local_work() {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let root = work_root(temp.path(), &id).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("generation.json"), "{damaged").unwrap();
        assert!(current_container_work_exists_with(
            temp.path(),
            Some(&domain_fixture("pod-new"))
        ));
    }

    #[test]
    fn completed_generation_receipt_outlives_command_diagnostics_but_not_a_live_guardian() {
        let (temp, id) = stuck_scope(None, None);
        let root = work_root(temp.path(), &id).unwrap();
        std::fs::write(root.join("commands/command.json"), "{damaged").unwrap();
        assert!(reconcile(temp.path()).is_err(), "Running is not exit proof");
        let mut value = generation(&root).unwrap();
        value.phase = GenerationPhase::Quiescent;
        save(&root.join("generation.json"), &value).unwrap();
        let held = lock(&root.join("generation.lock")).unwrap();
        assert!(verify_quiescent(temp.path(), &id).is_err());
        assert!(reconcile(temp.path()).is_err());
        drop(held);
        verify_quiescent(temp.path(), &id).unwrap();
        reconcile(temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("commands/command.json")).unwrap(),
            "{damaged"
        );
    }

    #[test]
    fn process_epoch_change_retires_only_local_generations_and_preserves_outcomes() {
        // Each guard overrides only this synchronous test thread.
        // (a) native scope, epoch changed → retired, business record untouched.
        let (_temp, id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        reconcile(_temp.path()).unwrap();
        let retired = generation(&work_root(_temp.path(), &id).unwrap()).unwrap();
        assert_eq!(retired.phase, GenerationPhase::Quiescent);
        assert_eq!(retired.exit_code, None, "no exit code is invented");
        assert!(
            retired
                .error
                .unwrap()
                .contains("business outcomes remain unchanged")
        );
        let command: serde_json::Value = read(
            &work_root(_temp.path(), &id)
                .unwrap()
                .join("commands/command.json"),
        )
        .unwrap();
        assert_eq!(command["phase"], "Quiescent");
        assert_eq!(command["termination"], "PhysicalDomainExited");
        assert_eq!(command["identity"]["task_id"], "original");
        drop(_guard);

        // (b) native scope, same epoch → still unconfirmed (hard kill only).
        let (_temp, id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(Some("pid1:11111111-1111-4111-8111-111111111111:100".into()));
        #[cfg(unix)]
        {
            let root = work_root(_temp.path(), &id).unwrap();
            let mut value = generation(&root).unwrap();
            let mut child = std::process::Command::new("true").spawn().unwrap();
            value.worker_pid = Some(child.id());
            child.wait().unwrap();
            save(&root.join("generation.json"), &value).unwrap();
        }
        // A dead worker is now eligible for cleanup, but its unknown command
        // outcome still prevents publishing quiescence or launching new work.
        #[cfg(unix)]
        {
            let candidate = reconcile(_temp.path()).unwrap().unwrap();
            assert!(
                process_utils::command_context::require_quiescent(&candidate.root.join("commands"))
                    .is_err()
            );
            assert_eq!(
                generation(&candidate.root).unwrap().phase,
                GenerationPhase::Running
            );
            drop(candidate);
        }
        #[cfg(not(unix))]
        assert!(reconcile(_temp.path()).is_err());
        assert!(verify_quiescent(_temp.path(), &id).is_err());
        drop(_guard);

        // (c) legacy record without epoch → conservative.
        let (_temp, _id) = stuck_scope(None, None);
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        assert!(reconcile(_temp.path()).is_err());
        drop(_guard);

        // (d) unreadable epoch → conservative.
        let (_temp, _id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(None);
        assert!(reconcile(_temp.path()).is_err());
        drop(_guard);
    }

    #[test]
    fn replacement_container_preserves_foreign_history_without_blocking_local_recovery() {
        let old = crate::domain::PhysicalDomain {
            authority: "test-runtime".into(),
            instance_source_env: None,
            instance: "old-container".into(),
            volume: "same-workspace-volume".into(),
        };
        let (temp, id) = stuck_scope(Some(old.clone()), None);
        let _owner = crate::Owner::try_acquire(temp.path()).unwrap().unwrap();
        let root = work_root(temp.path(), &id).unwrap();
        let original = std::fs::read(root.join("generation.json")).unwrap();
        let command = std::fs::read(root.join("commands/command.json")).unwrap();
        // Neither another container nor a missing local process is exit proof.
        assert!(reconcile_local(temp.path(), Some(&old)).is_err());
        assert!(reconcile_local(temp.path(), None).is_err());
        let mut current = old.clone();
        current.instance = "replacement-container".into();
        reconcile_local(temp.path(), Some(&current)).unwrap();
        assert_eq!(
            std::fs::read(root.join("generation.json")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(root.join("commands/command.json")).unwrap(),
            command
        );
        assert!(
            verify_quiescent(temp.path(), &id).is_err(),
            "no fabricated exit receipt"
        );
        current.volume = "another-volume".into();
        assert!(reconcile_local(temp.path(), Some(&current)).is_err());
        current.volume = old.volume;
        current.authority = "another-runtime".into();
        assert!(reconcile_local(temp.path(), Some(&current)).is_err());
    }

    #[test]
    fn local_epoch_never_retires_another_physical_domain() {
        use crate::domain::PhysicalDomain;
        let other = PhysicalDomain {
            authority: "daemon-a".into(),
            instance_source_env: None,
            volume: "volume-a".into(),
            instance: uuid::Uuid::new_v4().to_string(),
        };
        let current = other.clone();
        let same_domain = Generation {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            supervisor: "s".into(),
            token: "t".into(),
            intent: Intent::Run,
            phase: GenerationPhase::Running,
            worker_pid: None,
            exit_code: None,
            error: None,
            physical_domain: Some(other.clone()),
            process_epoch: Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        };
        // Same container identity, new incarnation → local proof applies.
        assert!(process_space_ended_with(&same_domain, Some(&current)));
        // A generation from a different container/pod needs platform evidence.
        let mut foreign = same_domain.clone();
        foreign.physical_domain = Some(PhysicalDomain {
            authority: "daemon-a".into(),
            instance_source_env: None,
            volume: "volume-a".into(),
            instance: uuid::Uuid::new_v4().to_string(),
        });
        assert!(!process_space_ended_with(&foreign, Some(&current)));
        // No current domain (native reader) cannot speak for stamped records.
        assert!(!process_space_ended_with(&foreign, None));
    }
}

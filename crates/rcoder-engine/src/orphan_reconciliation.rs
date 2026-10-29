//! Reverse inventory for empty remnants and confirmed-idle Docker agents.
//! Discovery never adopts business identity. Candidate
//! continuity is local to one leader generation and is never a startup gate.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use container_runtime_api::{
    AgentResourceEvent, AgentResourceIdentity, AgentStopOutcome, AgentStopSnapshot,
};
use rcoder_storage::{WorkloadAdmissionGuard, WorkloadAdmissionMode, WorkloadKey};
use shared_types::ServiceType;
use tokio::{sync::broadcast, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    app_state::AppState,
    config::{CleanupConfigSettings, OrphanReconcileMode},
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    Registered,
    LifecycleReferenced,
    LiveOrUnknown,
    DockerObserveOnly,
    /// Managed data is not proven to survive container removal (no persistent
    /// mount evidence); automatic retirement must never run for it.
    DataProtectionRequired,
    /// The UserApp authority references this live builder but its canonical
    /// project registration was missing; an identity-bound receipt or the
    /// current-lifecycle binding completed it, so the ordinary scanner can now
    /// evaluate it. No compute was created, started or restarted.
    RegistrationRecovered,
    /// The registration gap could not be closed from identity evidence this
    /// round. The resource is retained with a specific diagnostic: missing
    /// evidence is neither permission to adopt nor permission to delete.
    RegistrationUnconfirmed,
    /// The agent reported real activity this round; observation continuity
    /// resets and the idle window restarts from the next confirmed idle.
    ActivityBusy,
    WithinProtection,
    ObservationIncomplete,
    Observing,
    ObserveOnly,
    Superseded,
    Reclaimed,
    ServicesPending,
}

#[derive(Clone)]
struct Candidate {
    snapshot: AgentStopSnapshot,
    since: Instant,
    last: Instant,
    observations: u32,
}

impl Candidate {
    fn observe(
        previous: Option<&Self>,
        snapshot: &AgentStopSnapshot,
        now: Instant,
        interval: Duration,
    ) -> Self {
        match previous.filter(|old| old.snapshot == *snapshot) {
            Some(old) => Self {
                snapshot: snapshot.clone(),
                since: old.since,
                last: now,
                observations: if now.saturating_duration_since(old.last) >= interval {
                    old.observations.saturating_add(1)
                } else {
                    old.observations
                },
            },
            None => Self {
                snapshot: snapshot.clone(),
                since: now,
                last: now,
                observations: 1,
            },
        }
    }

    fn ready(&self, now: Instant, idle: Duration) -> bool {
        self.observations >= 2 && now.saturating_duration_since(self.since) >= idle
    }
}

/// Live-compute activity truth for a discovered Docker container. `Err` means
/// the state is unknown — never evidence of idleness — and must reset the
/// candidate's observation continuity.
enum Activity {
    Busy,
    Idle,
}

#[async_trait::async_trait]
trait ContainerActivityProbe: Send + Sync {
    async fn probe(&self, snapshot: &AgentStopSnapshot) -> Result<Activity>;
}

/// Production probe reuses the shared GetContainerStatus chain used by the
/// cleaner's secondary confirmation: the agent reports
/// `is_active || active_tasks > 0` regardless of caller identity, and the
/// query itself does not refresh activity, so probing cannot fake idleness
/// nor keep a container alive by observing it.
struct GrpcContainerActivityProbe {
    pool: Arc<crate::grpc::GrpcChannelPool>,
}

#[async_trait::async_trait]
impl ContainerActivityProbe for GrpcContainerActivityProbe {
    async fn probe(&self, snapshot: &AgentStopSnapshot) -> Result<Activity> {
        let endpoint = snapshot
            .endpoint
            .as_deref()
            .context("Activity probe requires an endpoint")?;
        let port = url::Url::parse(endpoint)?
            .port()
            .context("Activity endpoint has no RPC port")?;
        let grpc_addr = crate::utils::extract_grpc_addr_with_port(endpoint, port)
            .context("Activity probe endpoint is unreadable")?;
        // The agent-side response ignores caller identity; the identifier is
        // passed for request diagnostics only.
        let identifier = snapshot.identifier.as_str();
        let status = tokio::time::timeout(
            Duration::from_secs(3),
            crate::grpc::status_query::query_container_status(
                &self.pool,
                &grpc_addr,
                identifier,
                identifier,
                Duration::from_secs(3),
            ),
        )
        .await
        .context("Container activity deadline exceeded")??;
        // 旧 agent（证据版本 0）按产品决策与新 agent 同等自动回收：单节点
        // Docker 部署中用户流量均经主服务中转，观察窗内无活动即可判闲置，
        // 误回收由按需重建兜底。版本号不再作为拦截条件。
        if status.idle_observation_version == 0 {
            debug!(
                identifier,
                "Legacy agent activity evidence (idle_observation_version=0); auto-reclaim accepts is_active/active_tasks"
            );
        }
        let active = crate::grpc::status_query::is_agent_active(&status);
        if active {
            Ok(Activity::Busy)
        } else {
            Ok(Activity::Idle)
        }
    }
}

struct Scanner {
    state: Arc<AppState>,
    config: CleanupConfigSettings,
    activity_probe: Arc<dyn ContainerActivityProbe>,
    seen: HashMap<WorkloadKey, Candidate>,
    next_workload: Option<String>,
}

fn resources(snapshot: &AgentStopSnapshot) -> impl Iterator<Item = &AgentResourceIdentity> {
    snapshot
        .controller
        .iter()
        .chain(snapshot.pods.iter())
        .chain(snapshot.container.iter())
        .chain(snapshot.services.iter())
}

fn protected_shape(
    snapshot: &AgentStopSnapshot,
    now: DateTime<Utc>,
    protection: Duration,
) -> Option<Classification> {
    match snapshot.service_type {
        ServiceType::UserappBuilder
            if snapshot.app_id_hint.as_deref() != Some(snapshot.identifier.as_str()) =>
        {
            // Legacy collaborator identifiers are not the current pure-app
            // admission domain. Preserve them without guessing an app ID.
            return Some(Classification::ObservationIncomplete);
        }
        ServiceType::UserappBuilder
        | ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject => {}
        ServiceType::Userapp => return Some(Classification::LiveOrUnknown),
    }
    if let Some(container) = snapshot.container.as_ref() {
        // Live Docker compute. Ordinary agent families may retire compute after
        // activity observation; Builder and UserApp keep their own lifecycle
        // authorities and stay observe-only here.
        match snapshot.service_type {
            ServiceType::UserappBuilder | ServiceType::Userapp => {
                return Some(Classification::DockerObserveOnly);
            }
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject => {
                // Without an endpoint activity cannot be probed; without
                // StartedAt evidence a same-CID restart is undetectable.
                if snapshot.endpoint.is_none()
                    || container.uid.is_empty()
                    || container
                        .resource_version
                        .as_deref()
                        .is_none_or(str::is_empty)
                {
                    return Some(Classification::ObservationIncomplete);
                }
                if !snapshot
                    .compute_data_protection
                    .as_ref()
                    .is_some_and(|protection| protection.persistent_mounts)
                {
                    // `v: false` removal never restores writable-layer content;
                    // without a persistent mount the managed data dies with the
                    // container, so automatic retirement is forbidden.
                    return Some(Classification::DataProtectionRequired);
                }
                return match container.created_at {
                    None => Some(Classification::ObservationIncomplete),
                    Some(created) => match now.signed_duration_since(created).to_std() {
                        Ok(age) if age >= protection => None,
                        Ok(_) | Err(_) => Some(Classification::WithinProtection),
                    },
                };
            }
        }
    }
    if snapshot.namespace.is_none() {
        return Some(Classification::ObservationIncomplete);
    }
    if !snapshot.is_remnant() {
        return Some(Classification::LiveOrUnknown);
    }
    if snapshot.controller.is_none() && snapshot.services.is_empty() {
        return Some(Classification::ObservationIncomplete);
    }
    for resource in resources(snapshot) {
        if resource.uid.is_empty()
            || resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Some(Classification::ObservationIncomplete);
        }
        let Some(created) = resource.created_at else {
            return Some(Classification::ObservationIncomplete);
        };
        match now.signed_duration_since(created).to_std() {
            Ok(age) if age >= protection => {}
            Ok(_) | Err(_) => return Some(Classification::WithinProtection),
        }
    }
    None
}

impl Scanner {
    /// 缺失 canonical 项目登记的**存活** UserApp builder：普通 idle scanner 只
    /// 遍历 `projects`，因此它永远进不了回收链，而反向库存又因 UserApp 生命周期
    /// 引用一直保护它。这里在独立排他准入下用受控恢复补齐登记（只接受受身份
    /// 绑定的成功回执或当前生命周期绑定），再按登记状态正常评估。
    ///
    /// 不创建、不启动、不重启业务，不重放迁移；缺证据保留资源并给出明确分类。
    /// 返回 None = 本 workload 不需要这一步（非 builder / 已登记 / 无权威引用）。
    async fn complete_missing_builder_registration(
        &self,
        name: &str,
        key: &WorkloadKey,
        snapshot: &AgentStopSnapshot,
        deadline: Instant,
    ) -> Result<Option<Classification>> {
        if snapshot.service_type != ServiceType::UserappBuilder
            || (snapshot.controller.is_none() && snapshot.pods.is_empty())
        {
            return Ok(None);
        }
        // Legacy collaborator identifiers are not the current pure-app admission
        // domain (same rule `protected_shape` preserves them by): never guess an
        // app id for them, and never complete a registration from one.
        if snapshot.app_id_hint.as_deref() != Some(snapshot.identifier.as_str()) {
            return Ok(None);
        }
        let app_id = snapshot
            .app_id_hint
            .as_deref()
            .unwrap_or(&snapshot.identifier);
        if self
            .state
            .projects
            .capture_builder_registry_identity(app_id)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        // Only the UserApp authority keeps this workload: without it the
        // workload is an ordinary unreferenced remnant, not a registration gap.
        if !self.lifecycle_referenced(snapshot).await? {
            return Ok(None);
        }
        ensure!(
            Instant::now() < deadline,
            "Builder registration recovery budget exceeded"
        );
        // Admission precedes the local lifecycle lock, the same order as the
        // ensure and retirement paths. No guard is held here, so the exclusive
        // lease can never wait behind this scan's own shared observation.
        let guard = self
            .state
            .projects
            .acquire_workload_admission(key.clone(), WorkloadAdmissionMode::Exclusive, deadline)
            .await?;
        if guard.has_canonical_references().await? {
            return Ok(Some(Classification::Registered));
        }
        let Some(_local) = crate::userapp_builder::try_idle_builder_retirement(app_id).await else {
            debug!(app_id, workload = %name,
                "[ORPHAN_RECONCILIATION] builder registration recovery deferred; local lifecycle is busy");
            return Ok(Some(Classification::RegistrationUnconfirmed));
        };
        match crate::userapp_builder::recover_missing_builder_registration(&self.state, app_id)
            .await
        {
            Ok(Some(verified)) => {
                info!(app_id, workload = %name,
                    container_id = %verified.container_id, container_name = %verified.container_name,
                    "[ORPHAN_RECONCILIATION] missing builder registration completed from identity evidence");
                Ok(Some(Classification::RegistrationRecovered))
            }
            Ok(None) => {
                debug!(app_id, workload = %name,
                    "[ORPHAN_RECONCILIATION] builder registration gap unconfirmed; resource retained");
                Ok(Some(Classification::RegistrationUnconfirmed))
            }
            Err(error) => Err(error),
        }
    }

    async fn lifecycle_referenced(&self, snapshot: &AgentStopSnapshot) -> Result<bool> {
        match snapshot.service_type {
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject => Ok(false),
            ServiceType::Userapp => Ok(true),
            ServiceType::UserappBuilder => {
                // A missing projects row never bypasses the UserApp authority.
                let app_id = snapshot
                    .app_id_hint
                    .as_deref()
                    .unwrap_or(&snapshot.identifier);
                if self
                    .state
                    .userapp_store
                    .get_application(app_id)
                    .await?
                    .is_some()
                    || !self
                        .state
                        .userapp_store
                        .active_compute_controls(app_id)
                        .await?
                        .is_empty()
                {
                    return Ok(true);
                }
                for resource in resources(snapshot) {
                    if self
                        .state
                        .userapp_store
                        .get_resource_binding(&snapshot.service_type, &resource.uid)
                        .await?
                        .is_some()
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    async fn unreferenced(
        &self,
        guard: &WorkloadAdmissionGuard,
        snapshot: &AgentStopSnapshot,
    ) -> Result<Option<Classification>> {
        if guard.has_canonical_references().await? {
            return Ok(Some(Classification::Registered));
        }
        if self.lifecycle_referenced(snapshot).await? {
            return Ok(Some(Classification::LifecycleReferenced));
        }
        guard.verify_current().await?;
        Ok(None)
    }

    async fn consider(
        &self,
        key: &WorkloadKey,
        snapshot: &AgentStopSnapshot,
        candidate: &Candidate,
        deadline: Instant,
    ) -> Result<Classification> {
        let shape = protected_shape(
            snapshot,
            Utc::now(),
            Duration::from_secs(self.config.container_protection_seconds),
        );
        let ready = candidate.ready(
            Instant::now(),
            Duration::from_secs(self.config.long_idle_timeout_seconds),
        );
        let write = shape.is_none()
            && ready
            && self.config.orphan_reconcile_mode == OrphanReconcileMode::Reclaim;
        let mode = if write {
            WorkloadAdmissionMode::Exclusive
        } else {
            WorkloadAdmissionMode::Shared
        };
        let guard = self
            .state
            .projects
            .acquire_workload_admission(key.clone(), mode, deadline)
            .await?;
        if let Some(reason) = self.unreferenced(&guard, snapshot).await? {
            return Ok(reason);
        }
        if let Some(reason) = shape {
            return Ok(reason);
        }
        // Live Docker compute must prove idleness from the agent itself every
        // round; observation continuity only accrues on confirmed idle.
        if snapshot.container.is_some() {
            match self.activity_probe.probe(snapshot).await? {
                Activity::Busy => return Ok(Classification::ActivityBusy),
                Activity::Idle => {}
            }
        }
        if !ready {
            return Ok(Classification::Observing);
        }
        match self.config.orphan_reconcile_mode {
            OrphanReconcileMode::Observe => return Ok(Classification::ObserveOnly),
            OrphanReconcileMode::Reclaim => {}
        }
        // Refresh observations, never the deletion witness. A changed RV/UID or
        // a newly appeared Pod invalidates this entire candidate.
        let current = self
            .state
            .runtime()
            .capture_agent_stop(&snapshot.identifier, &snapshot.service_type)
            .await?;
        if current != *snapshot {
            return Ok(Classification::Superseded);
        }
        if protected_shape(
            &current,
            Utc::now(),
            Duration::from_secs(self.config.container_protection_seconds),
        )
        .is_some()
        {
            return Ok(Classification::Superseded);
        }
        // Exclusive recheck re-reads current activity so a request that began
        // after the shared observation can still cancel this retirement.
        if snapshot.container.is_some() {
            match self.activity_probe.probe(snapshot).await? {
                Activity::Busy => return Ok(Classification::ActivityBusy),
                Activity::Idle => {}
            }
        }
        // Last canonical query uses the pinned session owning the exact lock.
        if let Some(reason) = self.unreferenced(&guard, snapshot).await? {
            return Ok(reason);
        }
        ensure!(
            Instant::now() < deadline,
            "Orphan observation deadline exceeded before retirement"
        );
        match self.state.runtime().retire_agent_remnants(snapshot).await? {
            AgentStopOutcome::Complete => Ok(Classification::Reclaimed),
            AgentStopOutcome::ComputeGoneServicesPending { pending } => {
                for failure in pending {
                    warn!(service = %failure.name, uid = %failure.uid, error = %failure.error, "Orphan Service retirement remains pending");
                }
                Ok(Classification::ServicesPending)
            }
        }
    }

    async fn scan(&mut self) -> Result<()> {
        let started = Instant::now();
        // An incomplete inventory breaks continuity. A budget-limited scan
        // retains only entries still present unchanged in a complete inventory.
        // Deferred canonical checks do not increment their observation count.
        let previous = std::mem::take(&mut self.seen);
        let budget = Duration::from_secs(self.config.orphan_scan_budget_seconds.max(1));
        let deadline = started + budget;
        let inventory =
            tokio::time::timeout_at(deadline, self.state.runtime().agent_resource_inventory())
                .await
                .context("Runtime inventory timed out")??;
        let namespace = self.state.runtime().agent_workload_namespace();
        let mut inventory = inventory
            .into_iter()
            .map(|snapshot| {
                let name = self
                    .state
                    .runtime()
                    .agent_workload_name(&snapshot.identifier, &snapshot.service_type)?;
                let key = WorkloadKey::new(&namespace, snapshot.service_type, &name)?;
                Ok((name, key, snapshot))
            })
            .collect::<Result<Vec<_>>>()?;
        inventory.sort_by(|left, right| left.0.cmp(&right.0));
        if let Some(cursor) = &self.next_workload {
            let offset = inventory
                .iter()
                .position(|(name, _, _)| name >= cursor)
                .unwrap_or(0);
            inventory.rotate_left(offset);
        }
        for (_, key, snapshot) in &inventory {
            if let Some(candidate) = previous
                .get(key)
                .filter(|old| old.snapshot == *snapshot && snapshot.container.is_none())
            {
                self.seen.insert(key.clone(), candidate.clone());
            }
        }
        let discovered = inventory.len();
        let interval = Duration::from_secs(self.config.cleanup_interval_seconds.max(1));
        let mut visited = 0usize;
        let mut classified = 0usize;
        let mut failed = 0usize;
        let mut registrations = 0usize;
        self.next_workload = None;
        for (index, (name, key, snapshot)) in inventory.iter().enumerate() {
            if Instant::now() >= deadline {
                self.next_workload = Some(name.clone());
                break;
            }
            visited += 1;
            self.seen.remove(key);
            if snapshot
                .namespace
                .as_deref()
                .is_some_and(|actual| actual != namespace)
            {
                failed += 1;
                warn!(identifier = %snapshot.identifier,
                    "Inventory namespace differs from admission scope; preserving resources");
                continue;
            }
            // Close a missing canonical registration before evaluating this
            // workload, so a recovered builder is judged by its registration
            // (and enters the ordinary scanner) instead of being protected
            // forever as a lifecycle reference.
            match self
                .complete_missing_builder_registration(name, key, snapshot, deadline)
                .await
            {
                Ok(Some(Classification::RegistrationRecovered)) => registrations += 1,
                Ok(Some(Classification::RegistrationUnconfirmed)) => {}
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => {
                    failed += 1;
                    warn!(workload = %name, error = %format!("{error:#}"),
                        "Builder registration recovery is unknown; resource retained");
                }
            }
            let candidate =
                Candidate::observe(previous.get(key), snapshot, Instant::now(), interval);
            let result = tokio::time::timeout_at(
                deadline,
                self.consider(key, snapshot, &candidate, deadline),
            )
            .await;
            match result {
                Ok(Ok(classification)) => {
                    classified += 1;
                    let retain = match classification {
                        Classification::Observing | Classification::ObserveOnly => true,
                        // RegistrationRecovered / RegistrationUnconfirmed come from the
                        // pre-step above, never from `consider`; they are listed because
                        // the match must stay exhaustive over the enum.
                        Classification::Registered
                        | Classification::LifecycleReferenced
                        | Classification::LiveOrUnknown
                        | Classification::DockerObserveOnly
                        | Classification::DataProtectionRequired
                        | Classification::ActivityBusy
                        | Classification::WithinProtection
                        | Classification::ObservationIncomplete
                        | Classification::Superseded
                        | Classification::Reclaimed
                        | Classification::ServicesPending
                        | Classification::RegistrationRecovered
                        | Classification::RegistrationUnconfirmed => false,
                    };
                    if retain {
                        self.seen.insert(key.clone(), candidate);
                    }
                    match classification {
                        Classification::Registered => {
                            debug!(workload = %name, ?classification, "Runtime registration is present")
                        }
                        Classification::LifecycleReferenced
                        | Classification::LiveOrUnknown
                        | Classification::DockerObserveOnly
                        | Classification::DataProtectionRequired
                        | Classification::ActivityBusy
                        | Classification::WithinProtection
                        | Classification::ObservationIncomplete
                        | Classification::Observing
                        | Classification::ObserveOnly
                        | Classification::Superseded
                        | Classification::Reclaimed
                        | Classification::ServicesPending
                        | Classification::RegistrationRecovered
                        | Classification::RegistrationUnconfirmed => {
                            info!(workload = %name, identifier = %snapshot.identifier,
                                family = %snapshot.service_type, namespace = %namespace,
                                ?classification, "[ORPHAN_RECONCILIATION] runtime resource assessment");
                        }
                    }
                }
                Ok(Err(error)) => {
                    failed += 1;
                    warn!(workload = %name, error = %format!("{error:#}"),
                        "Orphan observation is unknown; preserving resources and resetting candidate");
                }
                Err(error) => {
                    failed += 1;
                    // A dispatched retirement may have an unknown outcome.
                    // Retain its diagnostic; never claim this candidate observed.
                    warn!(workload = %name, %error,
                        "Orphan scan budget reached; current retirement outcome may be unknown");
                    self.next_workload = inventory
                        .get(index + 1)
                        .or_else(|| inventory.first())
                        .map(|(name, _, _)| name.clone());
                    break;
                }
            }
        }
        info!(discovered, visited, classified, failed, registrations,
            deferred = discovered.saturating_sub(visited),
            elapsed_ms = started.elapsed().as_millis() as u64,
            budget_ms = budget.as_millis() as u64,
            next_workload = ?self.next_workload,
            "[ORPHAN_RECONCILIATION] scan coverage");
        Ok(())
    }
}

pub(crate) fn start(
    state: Arc<AppState>,
    config: CleanupConfigSettings,
    mut shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let cancellation = CancellationToken::new();
        let cancel_on_drop = cancellation.clone().drop_guard();
        let interval = Duration::from_secs(config.cleanup_interval_seconds.max(1));
        let events = state
            .runtime()
            .agent_resource_events(cancellation, interval)
            .await;
        let (mut receiver, watcher) = match events {
            Ok(events) => (Some(events.receiver), Some(events.task)),
            Err(error) => {
                debug!(%error, "Runtime Controller notifications unavailable; using complete periodic inventory");
                (None, None)
            }
        };
        let activity_probe = Arc::new(GrpcContainerActivityProbe {
            pool: state.grpc_pool.clone(),
        });
        let mut scanner = Scanner {
            state,
            config,
            activity_probe,
            seen: HashMap::new(),
            next_workload: None,
        };
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.recv() => break,
                event = async { match receiver.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    match event {
                        Some(AgentResourceEvent::Changed) => {
                            if Instant::now() < next { continue; }
                        }
                        Some(AgentResourceEvent::ObservationFailed(error)) => {
                            scanner.seen.clear();
                            warn!(%error, "Runtime Controller observation failed; orphan continuity reset");
                            continue;
                        }
                        None => {
                            warn!("Runtime Controller ended; restarting reconciliation through the leader supervisor");
                            break;
                        }
                    }
                }
                _ = tick.tick() => {}
            }
            if let Err(error) = scanner.scan().await {
                warn!(%error, "[ORPHAN_RECONCILIATION] scan incomplete; no absence inferred");
            }
            next = Instant::now() + interval;
        }
        drop(cancel_on_drop);
        if let Some(watcher) = watcher
            && let Err(error) = watcher.await
        {
            warn!(%error, "Runtime Controller task failed during drain");
        }
    })
}

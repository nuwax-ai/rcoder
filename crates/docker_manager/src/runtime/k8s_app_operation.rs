//! Per-application operation lease on the native `coordination.k8s.io` Lease
//! object. The holder renews `renewTime` on a cadence; a lease whose renewal
//! stopped for `LEASE_TTL_SECONDS` is expired and any replica may take it over
//! with a resourceVersion-conditioned patch (exactly one taker wins the CAS).
//! Crash residue is therefore bounded by the TTL instead of requiring operator
//! recovery. The explicit completion path still deletes the lease under
//! uid + live resourceVersion preconditions; a dropped incomplete holder only
//! stops renewing — the object is left for expiry so a successor can never
//! race a late API-server write from the dead holder. Mutation-layer fencing
//! (uid/resourceVersion preconditions plus durable admission) remains the
//! final guard regardless of lease state.
//!
//! Migration: acquisition only ever creates Lease objects. Legacy ConfigMap
//! leases with the same name are released through the ConfigMap fallback in
//! the captured validate/release paths (bound-terminal ones via the recovery
//! scanner, unbound ones via the orphan sweep), after which the fallback can
//! be retired. During the rolling upgrade window an old replica may still
//! hold a ConfigMap while a new replica acquires the Lease for the same app —
//! concurrent operations remain impossible because durable PG admission is
//! the first serialization layer for every mutating path.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Api,
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions},
};
use shared_types::{AppOperationLease, ServiceType};

/// Lease TTL: a holder that stops renewing for this long is considered dead
/// and any replica may take the lease over. Must exceed the longest plausible
/// in-flight API-server write window (~30s HTTP timeouts) so a takeover never
/// races a late write from the dead holder.
const LEASE_TTL_SECONDS: i32 = 60;
/// Renewal cadence (client-go leaderelection convention: TTL / 3). Two missed
/// renewals are tolerated before another holder may consider the lease dead.
const RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

struct OperationLease {
    api: Api<Lease>,
    name: String,
    uid: String,
    token: String,
    released: bool,
    cancel: tokio_util::sync::CancellationToken,
    receipt: shared_types::UserAppOperationLeaseReceipt,
}

/// Holder identity of a lease: the durable operation id when present, else the
/// legacy token. Reads exactly what acquisition writes, in the same priority.
fn holder_token(object: &Lease) -> Option<String> {
    let annotations = object.metadata.annotations.as_ref()?;
    annotations
        .get("rcoder.io/operation-id")
        .or_else(|| annotations.get("rcoder.io/legacy-operation-id"))
        .filter(|token| !token.is_empty())
        .cloned()
}

fn lease_now() -> k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(k8s_openapi::jiff::Timestamp::now())
}

/// Expiry verdict at an injectable observation time. Missing spec or renewTime
/// (and an unparseable timestamp is impossible by construction — the field is
/// a typed timestamp) counts as expired: an uninterpretable lease must not
/// fence forever, and mutation fencing guards any takeover mistake.
fn lease_expired_at(now: k8s_openapi::jiff::Timestamp, object: &Lease) -> bool {
    let Some(spec) = object.spec.as_ref() else {
        return true;
    };
    let ttl = spec.lease_duration_seconds.unwrap_or(LEASE_TTL_SECONDS);
    let Some(renew) = spec.renew_time.as_ref() else {
        return true;
    };
    // Timestamp subtraction yields a Span whose seconds field is the whole
    // age; a skew-driven negative age simply never exceeds the TTL.
    (now - renew.0).get_seconds() > i64::from(ttl)
}

/// Renewal body: only the observed resourceVersion (CAS anchor) and a fresh
/// renewTime are written. Built as a partial typed `Lease` so the timestamp
/// goes through `MicroTime`'s serializer, which is fixed to microsecond
/// precision (`%.6f`) — hand-stringified jiff stamps carry nanoseconds and the
/// apiserver rejects them (2026-09-22 app-154 lockup).
fn renewal_patch(resource_version: &str) -> ContainerRuntimeResult<serde_json::Value> {
    let patch = Lease {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            resource_version: Some(resource_version.to_owned()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            renew_time: Some(lease_now()),
            ..Default::default()
        }),
    };
    serde_json::to_value(patch).map_err(|error| {
        ContainerRuntimeError::K8sError(format!("serialize lease renewal patch: {error}"))
    })
}

/// Takeover body for an expired lease: rewrites holder identity, annotations
/// and labels to the taker's, bumps transitions, and is anchored on the
/// observed resourceVersion so exactly one competing taker can win. Typed
/// serialization for the same timestamp-precision reason as [`renewal_patch`];
/// serde skips `None` fields so the merge patch stays minimal.
fn takeover_patch(
    desired: &Lease,
    resource_version: &str,
    transitions: i32,
) -> ContainerRuntimeResult<serde_json::Value> {
    let holder_identity = desired
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.clone())
        .filter(|identity| !identity.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("Lease holder identity is missing".into())
        })?;
    let patch = Lease {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            resource_version: Some(resource_version.to_owned()),
            labels: desired.metadata.labels.clone(),
            annotations: desired.metadata.annotations.clone(),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(holder_identity),
            lease_duration_seconds: Some(LEASE_TTL_SECONDS),
            acquire_time: Some(lease_now()),
            renew_time: Some(lease_now()),
            lease_transitions: Some(transitions.saturating_add(1)),
            ..Default::default()
        }),
    };
    serde_json::to_value(patch).map_err(|error| {
        ContainerRuntimeError::K8sError(format!("serialize lease takeover patch: {error}"))
    })
}

enum RenewOutcome {
    Renewed,
    /// The holder definitively no longer owns the lease.
    Lost(String),
    /// Transient transport failure; retry on the next tick.
    Transient(String),
}

async fn renew_once(api: &Api<Lease>, name: &str, uid: &str, token: &str) -> RenewOutcome {
    let current = match api.get_opt(name).await {
        Ok(Some(current)) => current,
        Ok(None) => {
            return RenewOutcome::Lost(format!("operation lease {name} disappeared"));
        }
        Err(error) => {
            return RenewOutcome::Transient(format!("read operation lease {name}: {error}"));
        }
    };
    if current.metadata.uid.as_deref() != Some(uid)
        || holder_token(&current).as_deref() != Some(token)
    {
        return RenewOutcome::Lost(format!("operation lease {name} identity changed"));
    }
    let Some(version) = current
        .metadata
        .resource_version
        .clone()
        .filter(|version| !version.is_empty())
    else {
        return RenewOutcome::Lost(format!("operation lease {name} has no resourceVersion"));
    };
    let patch = match renewal_patch(&version) {
        Ok(patch) => Patch::Merge(patch),
        Err(error) => {
            return RenewOutcome::Transient(format!("build renewal patch {name}: {error}"));
        }
    };
    match api.patch(name, &PatchParams::default(), &patch).await {
        Ok(_) => RenewOutcome::Renewed,
        Err(kube::Error::Api(status)) if status.code == 409 => RenewOutcome::Lost(format!(
            "operation lease {name} renewal lost a concurrent update"
        )),
        Err(error) => RenewOutcome::Transient(format!("renew operation lease {name}: {error}")),
    }
}

/// Renewal heartbeat: runs until cancelled (the lease handle drops on release
/// or abandonment), keeping the lease alive while this process still operates.
fn spawn_renewal(
    api: Api<Lease>,
    name: String,
    uid: String,
    token: String,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RENEW_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // the first tick fires immediately; renew on cadence
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => match renew_once(&api, &name, &uid, &token).await {
                    RenewOutcome::Renewed => {}
                    RenewOutcome::Transient(error) => {
                        tracing::warn!(%error, "operation lease renewal failed; retrying on next tick");
                    }
                    RenewOutcome::Lost(error) => {
                        tracing::error!(%error,
                            "operation lease renewal lost; stopping renewal — expiry bounds the residue, mutation fencing still guards writes");
                        break;
                    }
                },
            }
        }
    });
}

/// Delete the Lease we own; identity is uid + holder token, and the delete
/// precondition uses the freshly observed resourceVersion (renewals move it,
/// so the acquisition-time receipt version is stale by design).
/// `Ok(false)` = no Lease object exists (legacy ConfigMap fallback applies).
async fn remove_owned_lease(
    api: &Api<Lease>,
    name: &str,
    uid: &str,
    token: &str,
) -> Result<bool, String> {
    let current = match api.get_opt(name).await {
        Ok(Some(current)) => current,
        Ok(None) => return Ok(false),
        Err(error) => return Err(format!("read application operation {name}: {error}")),
    };
    if current.metadata.uid.as_deref() != Some(uid)
        || holder_token(&current).as_deref() != Some(token)
    {
        return Err(format!(
            "release application operation {name}: lease was taken over"
        ));
    }
    let Some(version) = current
        .metadata
        .resource_version
        .as_deref()
        .filter(|version| !version.is_empty())
    else {
        return Err(format!(
            "release application operation {name}: lease has no resourceVersion"
        ));
    };
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid.into()),
            resource_version: Some(version.into()),
        }),
        ..Default::default()
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(true),
        Err(error) => Err(format!("release application operation {name}: {error}")),
    }
}

async fn remove_owned_configmap(
    api: &Api<ConfigMap>,
    name: &str,
    uid: &str,
    version: &str,
) -> Result<(), String> {
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid.into()),
            resource_version: Some(version.into()),
        }),
        ..Default::default()
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(format!("release application operation {name}: {error}")),
    }
}

#[async_trait::async_trait]
impl AppOperationLease for OperationLease {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        Some(self.receipt.clone())
    }
    async fn release(mut self: Box<Self>) -> Result<(), String> {
        // Absent (Ok(false)) is still a released state: idempotent release.
        let result = remove_owned_lease(&self.api, &self.name, &self.uid, &self.token)
            .await
            .map(|_| ());
        self.released = true;
        self.cancel.cancel();
        result
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        // Stop renewing first: an abandoned holder must let the lease expire.
        // The object itself is NOT deleted here — a cancelled HTTP future may
        // still have an API-server write in flight, and deleting would let a
        // successor race that late write. The residue is bounded by the TTL.
        self.cancel.cancel();
        if self.released {
            return;
        }
        tracing::error!(name = %self.name, uid = %self.uid,
            "application operation lease renewal stopped after incomplete operation; it self-expires within the lease TTL");
    }
}

/// What a conflicting create resolved into.
enum TakeOver {
    /// Boxed: the Lease object is ~500 bytes and this enum crosses an await
    /// boundary per acquisition attempt.
    Acquired(Box<Lease>),
    InProgress(Option<String>),
}

/// Resolve a 409 from lease creation: a lease that is still being renewed
/// reports the holder's operation; an expired one is taken over with a
/// resourceVersion-conditioned patch (a competing taker winning that CAS
/// surfaces as InProgress, never as a retry loop).
async fn take_over_expired(
    api: &Api<Lease>,
    name: &str,
    desired: &Lease,
) -> ContainerRuntimeResult<TakeOver> {
    let Some(current) = api.get_opt(name).await.map_err(|error| {
        ContainerRuntimeError::K8sError(format!("read application operation {name}: {error}"))
    })?
    else {
        // Vanished between the conflicting create and this read: report the
        // conflict shape; the caller's next attempt recreates the lease.
        return Ok(TakeOver::InProgress(None));
    };
    let holder = holder_token(&current);
    if !lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
        return Ok(TakeOver::InProgress(holder));
    }
    let Some(version) = current
        .metadata
        .resource_version
        .as_deref()
        .filter(|version| !version.is_empty())
    else {
        return Ok(TakeOver::InProgress(holder));
    };
    let transitions = current
        .spec
        .as_ref()
        .and_then(|spec| spec.lease_transitions)
        .unwrap_or(0);
    let patch = takeover_patch(desired, version, transitions)?;
    match api
        .patch(name, &PatchParams::default(), &Patch::Merge(patch))
        .await
    {
        Ok(taken) => Ok(TakeOver::Acquired(Box::new(taken))),
        Err(kube::Error::Api(status)) if status.code == 409 => {
            // A competing taker won between our read and this patch. Re-read
            // once so the reported holder is the actual winner, not the dead
            // holder we just read; an unreadable object reports no identity.
            let winner = match api.get_opt(name).await {
                Ok(Some(current)) => holder_token(&current),
                _ => None,
            };
            Ok(TakeOver::InProgress(winner))
        }
        Err(error) => Err(ContainerRuntimeError::K8sError(format!(
            "take over expired application operation {name}: {error}"
        ))),
    }
}

impl KubernetesRuntime {
    pub(super) async fn validate_captured_application_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Kubernetes lease receipt is required".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Operation lease scope changed".into(),
            ));
        }
        // Lease first, legacy ConfigMap fallback second (same name, different
        // API group). Lease identity is uid + holder token; the receipt's
        // resourceVersion is advisory because renewals move it forward.
        let lease_api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        if let Some(current) = lease_api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })? {
            let identity_ok = current.metadata.uid.as_deref() == Some(uid.as_str())
                && holder_token(&current).as_deref() == Some(token.as_str())
                && current
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|values| values.get("rcoder.io/operation-app"))
                    .map(String::as_str)
                    == Some(context.app_id.as_str())
                && current
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|values| values.get("rcoder.io/operation-family"))
                    .map(String::as_str)
                    == Some(service_type.to_string().as_str());
            if !identity_ok {
                return Err(ContainerRuntimeError::Conflict(
                    "Captured operation lease identity changed".into(),
                ));
            }
            return Ok(true);
        }
        self.validate_captured_configmap(context, service_type, name, receipt)
            .await
    }

    /// Legacy ConfigMap lease validation (pre-Lease migration stock): strict
    /// uid + resourceVersion + token match — ConfigMap revisions never churn.
    async fn validate_captured_configmap(
        &self,
        context: &shared_types::UserAppExecutionContext,
        service_type: &ServiceType,
        name: &str,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        let (uid, resource_version, token) = match receipt {
            shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                uid,
                resource_version,
                token,
                ..
            } => (uid, resource_version, token),
            _ => {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Kubernetes lease receipt is required".into(),
                ));
            }
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })?
        else {
            return Ok(false);
        };
        if !configmap_identity_matches(
            &current,
            context,
            service_type,
            uid,
            resource_version,
            token,
        ) {
            return Err(ContainerRuntimeError::Conflict(
                "Captured operation lease identity changed".into(),
            ));
        }
        Ok(true)
    }

    pub(super) async fn release_captured_application_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            token,
            ..
        } = receipt
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Kubernetes lease receipt is required".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Operation lease scope changed".into(),
            ));
        }
        // Lease delete attempt doubles as the kind dispatch: a single read
        // resolves existence, identity and the live resourceVersion.
        let lease_api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        match remove_owned_lease(&lease_api, name, uid, token).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(ContainerRuntimeError::K8sError(error)),
        }
        self.release_captured_configmap(context, service_type, name, receipt)
            .await
    }

    /// Legacy ConfigMap release path for migration stock.
    async fn release_captured_configmap(
        &self,
        context: &shared_types::UserAppExecutionContext,
        service_type: &ServiceType,
        name: &str,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        let (uid, resource_version, token) = match receipt {
            shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                uid,
                resource_version,
                token,
                ..
            } => (uid, resource_version, token),
            _ => {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Kubernetes lease receipt is required".into(),
                ));
            }
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })?
        else {
            return Ok(());
        };
        if !configmap_identity_matches(
            &current,
            context,
            service_type,
            uid,
            resource_version,
            token,
        ) {
            return Err(ContainerRuntimeError::Conflict(
                "Captured operation lease identity changed".into(),
            ));
        }
        remove_owned_configmap(&api, name, uid, resource_version)
            .await
            .map_err(ContainerRuntimeError::K8sError)
    }

    pub(super) async fn acquire_application_operation(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_application_operation_with_context(app_id, service_type, None)
            .await
    }

    /// Migration orphan sweep for legacy ConfigMap operation locks: acquire
    /// only creates Lease objects now, so any remaining `rcoder-operation-*`
    /// ConfigMap is pre-migration stock. Age-gated (24h ≫ any legitimate
    /// operation, and ≫ the rolling-upgrade window where old replicas still
    /// create ConfigMap locks) and uid-preconditioned; deleting one only
    /// releases a stale mutex and never authorizes a mutation. Concurrency
    /// safety does not rest on this sweep: durable PG admission remains the
    /// first serialization layer for every mutating path, so a swept lock
    /// cannot by itself let two operations through.
    pub(super) async fn sweep_legacy_operation_configmaps(
        &self,
        older_than: std::time::Duration,
    ) -> ContainerRuntimeResult<Vec<String>> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = api
            .list(&ListParams::default().labels("rcoder.io/operation-app"))
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("list legacy operation locks: {error}"))
            })?;
        let now = k8s_openapi::jiff::Timestamp::now();
        let threshold =
            k8s_openapi::jiff::SignedDuration::try_from(older_than).map_err(|error| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "legacy operation lock sweep window is invalid: {error}"
                ))
            })?;
        let mut deleted = Vec::new();
        for object in list.items {
            let Some(name) = object.metadata.name.clone() else {
                continue;
            };
            let Some(uid) = object.metadata.uid.clone() else {
                continue;
            };
            let Some(created) = object.metadata.creation_timestamp.as_ref() else {
                continue;
            };
            // Span seconds carry the whole age; younger than the threshold
            // (including skew-negative ages) is skipped. Second granularity
            // is plenty for a 24h-class sweep window.
            if (now - created.0).get_seconds() < threshold.as_secs() {
                continue;
            }
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid),
                    resource_version: object.metadata.resource_version.clone(),
                }),
                ..Default::default()
            };
            match api.delete(&name, &params).await {
                Ok(_) => {
                    tracing::info!(
                        name = %name,
                        "legacy ConfigMap operation lock swept (pre-Lease migration stock)"
                    );
                    deleted.push(name);
                }
                Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {
                    // Vanished or concurrently mutated: not ours to judge this
                    // round; the next sweep re-evaluates.
                }
                Err(error) => {
                    tracing::warn!(name = %name, %error, "legacy operation lock delete failed");
                }
            }
        }
        Ok(deleted)
    }

    pub(super) async fn acquire_application_operation_with_context(
        &self,
        app_id: &str,
        service_type: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        if !matches!(
            service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation lease only supports UserApp families".into(),
            ));
        }
        let name = operation_name(app_id, service_type)?;
        let mut annotations = std::collections::BTreeMap::new();
        let holder_identity;
        if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            annotations.insert(
                "rcoder.io/operation-id".into(),
                context.operation_id.clone(),
            );
            annotations.insert(
                "rcoder.io/lifecycle-id".into(),
                context.lifecycle_id.clone(),
            );
            annotations.insert("rcoder.io/executor-id".into(), context.executor_id.clone());
            annotations.insert(
                "rcoder.io/request-fingerprint".into(),
                context.request_fingerprint.clone(),
            );
            holder_identity = format!("{}:{}", context.executor_id, context.operation_id);
        } else {
            // Legacy locks are intentionally not advertised as durable operations.
            let legacy_id = uuid::Uuid::new_v4().to_string();
            annotations.insert("rcoder.io/legacy-operation-id".into(), legacy_id.clone());
            holder_identity = format!("legacy:{legacy_id}");
        }
        let token = annotations
            .get("rcoder.io/operation-id")
            .or_else(|| annotations.get("rcoder.io/legacy-operation-id"))
            .cloned()
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Operation token is missing".into())
            })?;
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let now = lease_now();
        let object = Lease {
            metadata: kube::api::ObjectMeta {
                name: Some(name.clone()),
                labels: Some(std::collections::BTreeMap::from([
                    (
                        "rcoder.io/operation-family".into(),
                        service_type.to_string(),
                    ),
                    ("rcoder.io/operation-app".into(), app_id.into()),
                ])),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(holder_identity),
                lease_duration_seconds: Some(LEASE_TTL_SECONDS),
                acquire_time: Some(now.clone()),
                renew_time: Some(now),
                lease_transitions: Some(0),
                ..Default::default()
            }),
        };
        let acquired = match api.create(&PostParams::default(), &object).await {
            Ok(created) => created,
            Err(kube::Error::Api(status)) if status.code == 409 => {
                match take_over_expired(&api, &name, &object).await? {
                    TakeOver::Acquired(taken) => *taken,
                    TakeOver::InProgress(operation_id) => {
                        // Report the live holder's identity, never this losing
                        // request's UUID; None means an unidentifiable holder.
                        return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                            shared_types::UserAppOperationInProgress {
                                app_id: app_id.into(),
                                service_type: *service_type,
                                resource_name: name,
                                operation_id,
                            },
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "acquire application operation {name}: {error}"
                )));
            }
        };
        let uid = acquired
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "acquired operation lease has no UID".into(),
                )
            })?;
        let version = acquired
            .metadata
            .resource_version
            .clone()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "acquired operation lease has no version".into(),
                )
            })?;
        let cancel = tokio_util::sync::CancellationToken::new();
        spawn_renewal(
            api.clone(),
            name.clone(),
            uid.clone(),
            token.clone(),
            cancel.clone(),
        );
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: *service_type,
            namespace: self.namespace.clone(),
            name: name.clone(),
            uid: uid.clone(),
            resource_version: version,
            token: token.clone(),
        };
        Ok(Box::new(OperationLease {
            receipt,
            api,
            name,
            uid,
            token,
            released: false,
            cancel,
        }))
    }
}

/// Legacy ConfigMap identity: strict uid + resourceVersion + token + labels.
/// ConfigMap revisions never churn, so the stored resourceVersion stays
/// authoritative for the migration stock.
fn configmap_identity_matches(
    current: &ConfigMap,
    context: &shared_types::UserAppExecutionContext,
    service_type: &ServiceType,
    uid: &str,
    resource_version: &str,
    token: &str,
) -> bool {
    let observed_token = current.metadata.annotations.as_ref().and_then(|values| {
        values
            .get("rcoder.io/operation-id")
            .or_else(|| values.get("rcoder.io/legacy-operation-id"))
    });
    current.metadata.uid.as_deref() == Some(uid)
        && current.metadata.resource_version.as_deref() == Some(resource_version)
        && observed_token.is_some_and(|observed| observed == token)
        && current
            .metadata
            .labels
            .as_ref()
            .and_then(|values| values.get("rcoder.io/operation-app"))
            .map(String::as_str)
            == Some(context.app_id.as_str())
        && current
            .metadata
            .labels
            .as_ref()
            .and_then(|values| values.get("rcoder.io/operation-family"))
            .map(String::as_str)
            == Some(service_type.to_string().as_str())
}

fn operation_name(app_id: &str, family: &ServiceType) -> ContainerRuntimeResult<String> {
    let code = match family {
        ServiceType::Userapp => "prod",
        ServiceType::UserappBuilder => "builder",
        _ => {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation requires UserApp family".into(),
            ));
        }
    };
    Ok(format!("rcoder-operation-{code}-{app_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_names_separate_prefix_shaped_app_ids_across_families() {
        assert_ne!(
            operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
            operation_name("foo", &ServiceType::UserappBuilder).expect("builder")
        );
        assert_eq!(
            operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
            "rcoder-operation-prod-builder-foo"
        );
    }

    /// 固定观察时刻，保证年龄构造与判定的基准一致（两次 now() 的微秒差
    /// 会让边界用例抖动）。
    fn fixed_now() -> k8s_openapi::jiff::Timestamp {
        "2026-09-21T12:00:00Z"
            .parse::<k8s_openapi::jiff::Timestamp>()
            .expect("fixed timestamp")
    }

    fn sample_lease(renew_age_secs: Option<i64>) -> Lease {
        let renew = renew_age_secs.map(|age| {
            let stamp = fixed_now() - k8s_openapi::jiff::SignedDuration::from_secs(age);
            k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(stamp)
        });
        Lease {
            metadata: kube::api::ObjectMeta {
                name: Some("rcoder-operation-prod-app".into()),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some("executor-one:operation-one".into()),
                lease_duration_seconds: Some(LEASE_TTL_SECONDS),
                acquire_time: None,
                renew_time: renew,
                lease_transitions: Some(0),
                ..Default::default()
            }),
        }
    }

    /// 过期判据：新鲜续租未过期；超过 TTL 过期；缺 renewTime/spec 过期。
    /// 修复前的行为（永不过期、operator recovery）正是事故根源。
    #[test]
    fn lease_expiry_requires_stale_renewal() {
        let now = fixed_now();
        assert!(!lease_expired_at(now, &sample_lease(Some(10))));
        assert!(!lease_expired_at(
            now,
            &sample_lease(Some(LEASE_TTL_SECONDS as i64))
        ));
        assert!(lease_expired_at(
            now,
            &sample_lease(Some(i64::from(LEASE_TTL_SECONDS) + 1))
        ));
        assert!(lease_expired_at(now, &sample_lease(None)));
        let mut missing_spec = sample_lease(Some(0));
        missing_spec.spec = None;
        assert!(lease_expired_at(now, &missing_spec));
    }

    /// TTL 覆盖缺省：对象未携带 leaseDurationSeconds 时用本实现常量判定。
    #[test]
    fn lease_expiry_falls_back_to_configured_ttl() {
        let mut object = sample_lease(Some(30));
        object.spec.as_mut().expect("spec").lease_duration_seconds = None;
        let now = fixed_now();
        assert!(!lease_expired_at(now, &object));
        let mut stale = sample_lease(Some(61));
        stale.spec.as_mut().expect("spec").lease_duration_seconds = None;
        assert!(lease_expired_at(now, &stale));
    }

    /// 持有者令牌优先级：durable operation-id 优先于 legacy 注解。
    #[test]
    fn holder_token_prefers_durable_operation_id() {
        let mut object = sample_lease(Some(0));
        object.metadata.annotations = Some(
            [
                ("rcoder.io/operation-id".to_string(), "op-1".to_string()),
                (
                    "rcoder.io/legacy-operation-id".to_string(),
                    "legacy-1".to_string(),
                ),
            ]
            .into(),
        );
        assert_eq!(holder_token(&object).as_deref(), Some("op-1"));
        object.metadata.annotations = Some(
            [(
                "rcoder.io/legacy-operation-id".to_string(),
                "legacy-1".to_string(),
            )]
            .into(),
        );
        assert_eq!(holder_token(&object).as_deref(), Some("legacy-1"));
        object.metadata.annotations =
            Some([("rcoder.io/operation-id".to_string(), String::new())].into());
        assert_eq!(holder_token(&object), None);
    }

    /// 续租体只带 CAS 锚（观察到的 resourceVersion）与新鲜 renewTime。
    #[test]
    fn renewal_patch_carries_cas_anchor_and_fresh_renew() {
        let body = renewal_patch("42").expect("patch");
        assert_eq!(body["metadata"]["resourceVersion"], "42");
        assert!(
            body["spec"]["renewTime"]
                .as_str()
                .is_some_and(|stamp| !stamp.is_empty())
        );
    }

    /// 接管体带 CAS 锚、持有者身份改写与 transitions 递增。
    #[test]
    fn takeover_patch_rewrites_identity_and_bumps_transitions() {
        let desired = sample_lease(Some(0));
        let body = takeover_patch(&desired, "7", 3).expect("patch");
        assert_eq!(body["metadata"]["resourceVersion"], "7");
        assert_eq!(body["spec"]["holderIdentity"], "executor-one:operation-one");
        assert_eq!(body["spec"]["leaseTransitions"], 4);
        assert_eq!(body["spec"]["leaseDurationSeconds"], LEASE_TTL_SECONDS);
    }

    /// 事故回归闸（2026-09-22 app-154 死锁）：patch 体时间戳必须恰好 6 位
    /// 微秒小数。手拼 `Timestamp::to_string()` 是纳秒（9 位），apiserver 的
    /// MicroTime 解析直接拒收（"cannot parse 488Z as Z07:00"）→ 续租/接管
    /// 永不成功 → 操作围栏死锁。类型化序列化（MicroTime serde 写死 %.6f）
    /// 是正确性的来源；本测试把该不变量锁死，防止退化回手拼。
    #[test]
    fn patch_timestamps_are_microsecond_precision() {
        /// RFC3339 时间戳小数位须恰好 6 位（`…T12:34:56.123456Z`）。
        fn assert_micros(field: &str, stamp: &str) {
            let fraction = stamp
                .rsplit_once('.')
                .map(|(_, tail)| tail.strip_suffix('Z').unwrap_or(tail))
                .unwrap_or_default();
            assert_eq!(
                fraction.chars().count(),
                6,
                "{field} must be exactly microsecond precision, got: {stamp}"
            );
            assert!(
                fraction.chars().all(|c| c.is_ascii_digit()),
                "{field} fraction must be digits, got: {stamp}"
            );
        }

        let renewal = renewal_patch("9").expect("renewal patch");
        assert_micros(
            "renewTime",
            renewal["spec"]["renewTime"].as_str().expect("renewTime"),
        );

        let desired = sample_lease(Some(0));
        let takeover = takeover_patch(&desired, "7", 0).expect("takeover patch");
        for field in ["acquireTime", "renewTime"] {
            let stamp = takeover["spec"][field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} missing in takeover patch: {takeover}"));
            assert_micros(field, stamp);
        }
    }
}

/// 线协议状态机测试：真 kube client + 本地确定性 apiserver 适配器
/// （与 kubernetes_runtime.rs create_lease_tests 同款模式）。覆盖 Lease 的
/// 过期接管 CAS、竞争失败回退、释放的实时 RV precondition 与 legacy
/// ConfigMap 迁移回退。
#[cfg(all(test, feature = "kubernetes"))]
mod wire_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn runtime_for(client: kube::Client) -> KubernetesRuntime {
        use crate::runtime::kubernetes_runtime::{KubernetesRuntime, KubernetesRuntimeConfig};
        KubernetesRuntime {
            client,
            namespace: "lease-test".into(),
            config: KubernetesRuntimeConfig {
                namespace: "lease-test".into(),
                cluster_domain: "cluster.local".into(),
                pod_ttl_seconds: None,
                image_pull_secret: None,
                service_account_name: "test".into(),
                nfs_server: "unused".into(),
                nfs_path: "/unused".into(),
                storage_class: "unused".into(),
                access_mode: "ReadWriteOnce".into(),
                docker_manager_config: Default::default(),
                kubernetes_config: Default::default(),
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
            event_publisher: Default::default(),
            event_counters: Arc::new(
                crate::runtime::k8s_event_publisher::PublisherCounters::default(),
            ),
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let (head, offset, length) = loop {
            let n = stream.read(&mut buffer).await.expect("read");
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().expect("length"))
                    })
                    .unwrap_or(0);
                break (head, offset + 4, length);
            }
        };
        while bytes.len() < offset + length {
            let n = stream.read(&mut buffer).await.expect("body");
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        (head, bytes[offset..offset + length].to_vec())
    }

    async fn write_reply(stream: &mut tokio::net::TcpStream, code: u16, body: &serde_json::Value) {
        let body = body.to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("response");
    }

    fn status(code: u16, message: &str) -> serde_json::Value {
        let reason = if code == 404 { "NotFound" } else { "Conflict" };
        serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "reason": reason, "code": code, "message": message,
        })
    }

    /// 构造一个 Lease JSON。renew 偏移秒数（相对 now）决定过期判定。
    fn lease_json(
        uid: &str,
        rv: &str,
        operation_id: Option<&str>,
        renew_age_secs: i64,
        transitions: i32,
    ) -> serde_json::Value {
        let renew = (k8s_openapi::jiff::Timestamp::now()
            - k8s_openapi::jiff::SignedDuration::from_secs(renew_age_secs))
        .to_string();
        let mut annotations = serde_json::Map::new();
        if let Some(operation) = operation_id {
            annotations.insert("rcoder.io/operation-id".into(), operation.into());
        } else {
            annotations.insert(
                "rcoder.io/legacy-operation-id".into(),
                "legacy-holder".into(),
            );
        }
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": {
                "name": "rcoder-operation-prod-takeover",
                "namespace": "lease-test",
                "uid": uid, "resourceVersion": rv,
                "labels": {
                    "rcoder.io/operation-app": "takeover",
                    "rcoder.io/operation-family": ServiceType::Userapp.to_string(),
                },
                "annotations": annotations,
            },
            "spec": {
                "holderIdentity": format!("executor:{operation_id:?}"),
                "leaseDurationSeconds": LEASE_TTL_SECONDS,
                "acquireTime": renew,
                "renewTime": renew,
                "leaseTransitions": transitions,
            },
        })
    }

    fn context_for(app_id: &str, operation_id: &str) -> shared_types::UserAppExecutionContext {
        shared_types::UserAppExecutionContext {
            app_id: app_id.into(),
            lifecycle_id: "lifecycle-one".into(),
            operation_id: operation_id.into(),
            executor_id: "executor-live".into(),
            request_fingerprint: "ab".repeat(32),
        }
    }

    async fn client_for(address: std::net::SocketAddr) -> kube::Client {
        drop(rustls::crypto::ring::default_provider().install_default());
        kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().expect("uri"),
        ))
        .expect("client")
    }

    /// 过期租约被 CAS 接管：POST 409 → GET（过期持有者）→ PATCH（断言 RV
    /// 锚 + 身份改写 + transitions 递增）→ 获得租约；release 再走 GET→
    /// DELETE（实时 RV precondition）。修复前语义（409 一律 OperationInProgress、
    /// 永不接管）正是"死持有者永久占锁"事故的根源。
    #[tokio::test]
    async fn expired_lease_is_taken_over_with_cas_and_releases() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("acquire POST");
                let (head, _) = read_request(&mut stream).await;
                assert!(
                    head.starts_with("POST ") && head.contains("/leases"),
                    "{head}"
                );
                write_reply(&mut stream, 409, &status(409, "lease exists")).await;

                let (mut stream, _) = listener.accept().await.expect("takeover GET");
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("GET "), "{head}");
                write_reply(
                    &mut stream,
                    200,
                    &lease_json("dead-holder", "9", Some("dead-operation"), 3600, 2),
                )
                .await;

                let (mut stream, _) = listener.accept().await.expect("takeover PATCH");
                let (head, body) = read_request(&mut stream).await;
                assert!(head.starts_with("PATCH "), "{head}");
                let patch: serde_json::Value = serde_json::from_slice(&body).expect("patch");
                assert_eq!(patch["metadata"]["resourceVersion"], "9", "CAS anchor");
                assert_eq!(patch["spec"]["leaseTransitions"], 3, "transitions bump");
                assert_eq!(
                    patch["metadata"]["annotations"]["rcoder.io/operation-id"], "live-operation",
                    "holder identity rewrite"
                );
                let mut taken = lease_json("dead-holder", "10", Some("live-operation"), 0, 3);
                taken["metadata"]["resourceVersion"] = "10".into();
                write_reply(&mut stream, 200, &taken).await;

                // release：GET（实时身份/RV）→ DELETE（uid + 实时 RV）。
                let (mut stream, _) = listener.accept().await.expect("release GET");
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("GET "), "{head}");
                write_reply(&mut stream, 200, &taken).await;

                let (mut stream, _) = listener.accept().await.expect("release DELETE");
                let (head, body) = read_request(&mut stream).await;
                assert!(head.starts_with("DELETE "), "{head}");
                let preconditions: serde_json::Value =
                    serde_json::from_slice(&body).expect("release body");
                assert_eq!(preconditions["preconditions"]["uid"], "dead-holder");
                assert_eq!(preconditions["preconditions"]["resourceVersion"], "10");
                write_reply(
                    &mut stream,
                    200,
                    &serde_json::json!({"apiVersion":"v1","kind":"Status",
                        "status":"Success","code":200}),
                )
                .await;
            });
            let context = context_for("takeover", "live-operation");
            let lease = runtime_for(client_for(address).await)
                .acquire_application_operation_with_context(
                    "takeover",
                    &ServiceType::Userapp,
                    Some(&context),
                )
                .await
                .expect("expired lease must be taken over");
            assert!(lease.receipt().is_some());
            lease.release().await.expect("release after takeover");
            server.await.expect("adapter assertions");
        })
        .await
        .expect("takeover scenario within budget");
    }

    /// 接管竞争失败（PATCH 409 = 别的副本先赢）：上报持有者身份的
    /// OperationInProgress，绝不重试抢占。
    #[tokio::test]
    async fn takeover_race_surfaces_holder_in_progress() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("acquire POST");
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("POST "), "{head}");
                write_reply(&mut stream, 409, &status(409, "lease exists")).await;

                let (mut stream, _) = listener.accept().await.expect("takeover GET");
                read_request(&mut stream).await;
                write_reply(
                    &mut stream,
                    200,
                    &lease_json("dead-holder", "9", Some("dead-operation"), 3600, 2),
                )
                .await;

                let (mut stream, _) = listener.accept().await.expect("takeover PATCH");
                read_request(&mut stream).await;
                write_reply(&mut stream, 409, &status(409, "another taker won")).await;

                // 竞争失败后重读一次：上报真赢家身份，而非刚读到的死持有者。
                let (mut stream, _) = listener.accept().await.expect("winner re-read");
                read_request(&mut stream).await;
                write_reply(
                    &mut stream,
                    200,
                    &lease_json("actual-winner", "11", Some("winning-operation"), 0, 4),
                )
                .await;
            });
            let context = context_for("takeover", "live-operation");
            let result = runtime_for(client_for(address).await)
                .acquire_application_operation_with_context(
                    "takeover",
                    &ServiceType::Userapp,
                    Some(&context),
                )
                .await;
            assert!(
                matches!(
                    &result,
                    Err(ContainerRuntimeError::OperationInProgress(operation))
                        if operation.operation_id.as_deref() == Some("winning-operation")
                ),
                "racing takeover must surface the actual winner's identity"
            );
            server.await.expect("adapter assertions");
        })
        .await
        .expect("takeover race within budget");
    }

    /// 迁移回退：无 Lease 对象时按 legacy ConfigMap 校验并释放（存量 PG
    /// 绑定里的旧 receipt 逐字节兼容）。
    #[tokio::test]
    async fn captured_release_falls_back_to_legacy_configmap() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                // 先 Lease 探测 → 404。
                let (mut stream, _) = listener.accept().await.expect("lease probe");
                let (head, _) = read_request(&mut stream).await;
                assert!(
                    head.contains("/leases/rcoder-operation-prod-legacy"),
                    "{head}"
                );
                write_reply(&mut stream, 404, &status(404, "not found")).await;

                // ConfigMap GET → 存量身份。
                let (mut stream, _) = listener.accept().await.expect("configmap read");
                let (head, _) = read_request(&mut stream).await;
                assert!(
                    head.contains("/configmaps/rcoder-operation-prod-legacy"),
                    "{head}"
                );
                write_reply(
                    &mut stream,
                    200,
                    &serde_json::json!({
                        "apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {
                            "name": "rcoder-operation-prod-legacy",
                            "namespace": "lease-test",
                            "uid": "legacy-uid", "resourceVersion": "5",
                            "labels": {
                                "rcoder.io/operation-app": "legacy",
                                "rcoder.io/operation-family": ServiceType::Userapp.to_string(),
                            },
                            "annotations": {"rcoder.io/legacy-operation-id": "legacy-token"},
                        },
                    }),
                )
                .await;

                let (mut stream, _) = listener.accept().await.expect("configmap delete");
                let (head, body) = read_request(&mut stream).await;
                assert!(head.starts_with("DELETE "), "{head}");
                let preconditions: serde_json::Value =
                    serde_json::from_slice(&body).expect("release body");
                assert_eq!(preconditions["preconditions"]["uid"], "legacy-uid");
                assert_eq!(preconditions["preconditions"]["resourceVersion"], "5");
                write_reply(
                    &mut stream,
                    200,
                    &serde_json::json!({"apiVersion":"v1","kind":"Status",
                        "status":"Success","code":200}),
                )
                .await;
            });
            let context = context_for("legacy", "legacy-operation");
            let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::Userapp,
                namespace: "lease-test".into(),
                name: "rcoder-operation-prod-legacy".into(),
                uid: "legacy-uid".into(),
                resource_version: "5".into(),
                token: "legacy-token".into(),
            };
            runtime_for(client_for(address).await)
                .release_captured_application_operation(&context, &receipt)
                .await
                .expect("legacy fallback release");
            server.await.expect("adapter assertions");
        })
        .await
        .expect("legacy fallback within budget");
    }

    /// 释放时的身份防线：Lease 已被别的持有者接管（uid 不符）→ 报错而非
    /// 删除别人的锁。
    #[tokio::test]
    async fn captured_release_rejects_taken_over_lease() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("lease probe");
                read_request(&mut stream).await;
                write_reply(
                    &mut stream,
                    200,
                    &lease_json("someone-else", "12", Some("other-operation"), 0, 4),
                )
                .await;
            });
            let context = context_for("takeover", "live-operation");
            let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::Userapp,
                namespace: "lease-test".into(),
                name: "rcoder-operation-prod-takeover".into(),
                uid: "dead-holder".into(),
                resource_version: "10".into(),
                token: "live-operation".into(),
            };
            let result = runtime_for(client_for(address).await)
                .release_captured_application_operation(&context, &receipt)
                .await;
            assert!(result.is_err(), "taken-over lease must not be deleted");
            server.await.expect("adapter assertions");
        })
        .await
        .expect("taken-over rejection within budget");
    }
}

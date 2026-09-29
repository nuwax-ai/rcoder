use super::*;

/// Lease TTL: a holder that stops renewing for this long is considered dead
/// and any replica may take the lease over. Must exceed the longest plausible
/// in-flight API-server write window (~30s HTTP timeouts) so a takeover never
/// races a late write from the dead holder.
pub(super) const LEASE_TTL_SECONDS: i32 = 60;
/// Renewal cadence (client-go leaderelection convention: TTL / 3). Two missed
/// renewals are tolerated before another holder may consider the lease dead.
const RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

pub(super) struct OperationLease {
    pub(super) api: Api<Lease>,
    pub(super) name: String,
    pub(super) uid: String,
    pub(super) token: String,
    pub(super) released: bool,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) receipt: shared_types::UserAppOperationLeaseReceipt,
}

/// Holder identity: compute attempt token, ordinary operation id, then legacy
/// token. Reads exactly what acquisition writes, in the same priority.
pub(super) fn holder_token(object: &Lease) -> Option<String> {
    let annotations = object.metadata.annotations.as_ref()?;
    annotations
        .get("rcoder.io/lease-token")
        .or_else(|| annotations.get("rcoder.io/operation-id"))
        .or_else(|| annotations.get("rcoder.io/legacy-operation-id"))
        .filter(|token| !token.is_empty())
        .cloned()
}

/// Diagnostics report the operation, while ownership uses the per-attempt token.
fn reported_operation(object: &Lease) -> Option<String> {
    object
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get("rcoder.io/operation-id"))
        .filter(|id| !id.is_empty())
        .cloned()
        .or_else(|| holder_token(object))
}

pub(super) fn lease_now() -> k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(k8s_openapi::jiff::Timestamp::now())
}

/// Expiry verdict at an injectable observation time. Missing spec or renewTime
/// (and an unparseable timestamp is impossible by construction — the field is
/// a typed timestamp) counts as expired: an uninterpretable lease must not
/// fence forever, and mutation fencing guards any takeover mistake.
pub(super) fn lease_expired_at(now: k8s_openapi::jiff::Timestamp, object: &Lease) -> bool {
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
pub(super) fn renewal_patch(resource_version: &str) -> ContainerRuntimeResult<serde_json::Value> {
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
pub(super) fn takeover_patch(
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
    let mut patch = serde_json::to_value(patch).map_err(|error| {
        ContainerRuntimeError::K8sError(format!("serialize lease takeover patch: {error}"))
    })?;
    // JSON merge preserves unspecified map keys. Explicitly retire the previous
    // holder's identity fields when switching between compute/business/legacy.
    for key in [
        "rcoder.io/lease-token",
        "rcoder.io/compute-lease",
        "rcoder.io/operation-id",
        "rcoder.io/legacy-operation-id",
        "rcoder.io/executor-id",
        "rcoder.io/lifecycle-id",
        "rcoder.io/request-fingerprint",
    ] {
        if !desired
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|values| values.contains_key(key))
        {
            patch["metadata"]["annotations"][key] = serde_json::Value::Null;
        }
    }
    Ok(patch)
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
pub(super) fn spawn_renewal(
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

/// Outcome of removing the Lease this receipt used to own.
pub(super) enum LeaseRemoval {
    /// Deleted by us with uid + freshly observed resourceVersion preconditions.
    Deleted,
    /// No Lease object exists (legacy ConfigMap fallback applies).
    Absent,
    /// uid/holder-token mismatch: a successor already took the object over.
    /// The old receipt holds no authority left — the successor's lock is not
    /// ours to delete, and nothing of ours still fences the old operation.
    TakenOver,
}

/// Pure polarity lock: an identity mismatch on a live Lease means takeover —
/// a released state for the old receipt — not a release error. Feeding it
/// into cleanup chains as an error caused permanent release/forget retries
/// and re-fenced completed operations (D2).
pub(super) fn lease_taken_over(current: &Lease, uid: &str, token: &str) -> bool {
    current.metadata.uid.as_deref() != Some(uid) || holder_token(current).as_deref() != Some(token)
}

/// Delete the Lease we own; identity is uid + holder token, and the delete
/// precondition uses the freshly observed resourceVersion (renewals move it,
/// so the acquisition-time receipt version is stale by design).
/// `Ok(Absent)` = no Lease object exists (legacy ConfigMap fallback applies);
/// `Ok(TakenOver)` = a successor owns the object now — also a released state.
pub(super) async fn remove_owned_lease(
    api: &Api<Lease>,
    name: &str,
    uid: &str,
    token: &str,
) -> Result<LeaseRemoval, String> {
    let current = match api.get_opt(name).await {
        Ok(Some(current)) => current,
        Ok(None) => return Ok(LeaseRemoval::Absent),
        Err(error) => return Err(format!("read application operation {name}: {error}")),
    };
    if lease_taken_over(&current, uid, token) {
        return Ok(LeaseRemoval::TakenOver);
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
        Ok(_) => Ok(LeaseRemoval::Deleted),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(LeaseRemoval::Deleted),
        Err(error) => Err(format!("release application operation {name}: {error}")),
    }
}

pub(super) async fn remove_owned_configmap(
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
        // Absent and TakenOver are both released states: idempotent release.
        // A taken-over lease belongs to the successor; this receipt has
        // nothing left to unlock.
        let outcome = remove_owned_lease(&self.api, &self.name, &self.uid, &self.token).await;
        if matches!(outcome, Ok(LeaseRemoval::TakenOver)) {
            tracing::warn!(name = %self.name, uid = %self.uid,
                "operation lease was taken over before release; the successor now owns it");
        }
        self.released = true;
        self.cancel.cancel();
        outcome.map(|_| ())
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
pub(super) enum TakeOver {
    /// Boxed: the Lease object is ~500 bytes and this enum crosses an await
    /// boundary per acquisition attempt.
    Acquired(Box<Lease>),
    InProgress(Option<String>),
}

/// Resolve a 409 from lease creation: a lease that is still being renewed
/// reports the holder's operation; an expired one is taken over with a
/// resourceVersion-conditioned patch (a competing taker winning that CAS
/// surfaces as InProgress, never as a retry loop).
pub(super) async fn take_over_expired(
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
    let holder = reported_operation(&current);
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
                Ok(Some(current)) => reported_operation(&current),
                _ => None,
            };
            Ok(TakeOver::InProgress(winner))
        }
        Err(error) => Err(ContainerRuntimeError::K8sError(format!(
            "take over expired application operation {name}: {error}"
        ))),
    }
}

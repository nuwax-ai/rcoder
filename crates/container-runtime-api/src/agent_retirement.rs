//! Compute retirement witnesses. These never contain a storage deletion target.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared_types::ServiceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentResourceKind {
    Container,
    StatefulSet,
    Pod,
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResourceIdentity {
    pub kind: AgentResourceKind,
    pub name: String,
    pub uid: String,
    pub resource_version: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    /// Captured controller identity; absence means a genuinely bare resource.
    pub owner_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStopSnapshot {
    /// Actual runtime namespace or stable Docker daemon scope.
    pub namespace: Option<String>,
    pub identifier: String,
    pub service_type: ServiceType,
    /// Unmodified, consistent `rcoder.io/app-id` label, when available.
    /// A missing or conflicting hint never grants Builder retirement authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id_hint: Option<String>,
    pub controller: Option<AgentResourceIdentity>,
    pub pods: Vec<AgentResourceIdentity>,
    pub container: Option<AgentResourceIdentity>,
    pub services: Vec<AgentResourceIdentity>,
    pub replicas: Option<i32>,
    pub endpoint: Option<String>,
    /// Evidence collected from the Docker engine for live compute: whether
    /// managed data sits on persistent mounts outside the writable layer.
    /// Absent on Kubernetes snapshots (PVC retention is structural there);
    /// policy decisions stay in the engine, never in the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_data_protection: Option<ComputeDataProtection>,
}

/// Writable-layer survival evidence for a live Docker container. `v: false`
/// removal never restores writable-layer content, so retirement must only run
/// when managed data is carried by mounts that outlive the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeDataProtection {
    /// The configured workspace is covered by its deployment-owned writable
    /// source/destination mount, without unproven nested shadow mounts.
    pub persistent_mounts: bool,
    /// Host-configured AutoRemove; container data on persistent mounts is
    /// unaffected, but it forecloses any writable-layer recovery flow.
    pub auto_remove: bool,
}

impl AgentResourceIdentity {
    /// A fresh capture may observe a status/fence RV update of the same object.
    /// This never relaxes the exact UID/RV comparison of a returned stop witness.
    pub fn same_resource(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.name == other.name
            && self.uid == other.uid
            && self.owner_uid == other.owner_uid
            && self.created_at == other.created_at
    }
}

impl AgentStopSnapshot {
    /// A controller without a Pod may still be rebuilding execution.
    pub fn has_compute(&self) -> bool {
        self.controller.is_some() || self.container.is_some() || !self.pods.is_empty()
    }

    pub fn oldest_created_at(&self) -> Option<DateTime<Utc>> {
        self.controller
            .iter()
            .chain(self.pods.iter())
            .chain(self.container.iter())
            .chain(self.services.iter())
            .filter_map(|resource| resource.created_at)
            .min()
    }

    pub fn is_remnant(&self) -> bool {
        self.container.is_none()
            && self.pods.is_empty()
            && (self.controller.is_none() || self.replicas == Some(0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentServiceCleanupFailure {
    pub name: String,
    pub uid: String,
    pub error: String,
}

/// An error from retirement means compute disappearance is NOT confirmed.
/// Service failures retain their original diagnostic after compute is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStopOutcome {
    Complete,
    ComputeGoneServicesPending {
        pending: Vec<AgentServiceCleanupFailure>,
    },
}

#[derive(Debug)]
pub enum AgentResourceEvent {
    Changed,
    ObservationFailed(String),
}

/// The caller owns both the receiver and the task, and must join on shutdown.
pub struct AgentResourceEvents {
    pub receiver: tokio::sync::mpsc::Receiver<AgentResourceEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

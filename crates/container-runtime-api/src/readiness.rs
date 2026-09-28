//! Read-only UserApp discovery and a fixed management query. These targets do
//! not grant mutation authority and never require an operation or runtime lease.

use std::net::SocketAddr;

use shared_types::{UserAppNoComputeState, UserappStage};

pub const USERAPP_READINESS_COMMAND: [&str; 5] = [
    "app-cli",
    "readiness",
    "--json",
    "--admin-addr",
    "127.0.0.1:3010",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAppReadinessInstance {
    Docker {
        container_id: String,
        /// Detect an in-place restart, which preserves the container ID and IP.
        started_at: Option<String>,
    },
    Kubernetes {
        namespace: String,
        pod_name: String,
        pod_uid: String,
        container_name: String,
        /// Detect a container restart inside the same Pod.
        container_id: Option<String>,
        owner_uid: String,
    },
}

impl UserAppReadinessInstance {
    pub fn physical_id(&self) -> &str {
        match self {
            Self::Docker { container_id, .. } => container_id,
            Self::Kubernetes { pod_uid, .. } => pod_uid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAppReadinessTarget {
    pub app_id: String,
    pub stage: UserappStage,
    pub instance: UserAppReadinessInstance,
    pub address: Option<SocketAddr>,
    pub published_address: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
// The running payload is already boxed. Padding/boxing the one-byte state
// would add work without making this small tagged pointer any smaller.
#[allow(variant_size_differences)]
pub enum UserAppRuntimeReadiness {
    NotRunning(UserAppNoComputeState),
    Running(Box<UserAppReadinessTarget>),
}

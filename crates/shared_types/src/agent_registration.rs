//! In-memory runtime evidence for registration-only Agent workload recovery.
//! These snapshots never authorize resource mutation or replace storage CAS.

use crate::{ContainerBasicInfo, ServiceType};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentWorkspaceIdentity {
    pub namespace: String,
    pub claim_name: String,
    pub claim_uid: String,
    pub mount_path: String,
    pub sub_path: Option<String>,
}

/// The runtime verifies current ownership and workspace, and, when provided,
/// confirms the captured predecessor is absent. A detached project has no old
/// physical identity to assert; attaching it authorizes registration only. Storage must
/// still compare the captured registration generation when committing it.
/// Deliberately has no persisted or public HTTP representation.
#[derive(Clone, Debug)]
pub struct VerifiedAgentReplacement {
    pub identifier: String,
    pub service_type: ServiceType,
    pub previous: Option<ContainerBasicInfo>,
    pub replacement: ContainerBasicInfo,
    pub workspace: AgentWorkspaceIdentity,
}

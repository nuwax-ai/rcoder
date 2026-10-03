//! Bounded, typed diagnostics for UserApp source admission and task failures.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppDiagnosticCode {
    WorkspaceEmpty,
    WorkspaceRootMismatch,
    WorkspaceManifestMissing,
    WorkspaceIo,
    NoServices,
    ManifestParse,
    ManifestValidation,
    OwnerPreflight,
    TaskCapacity,
    WorkerAdmission,
    BuildFailed,
    StartFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppDiagnosticPhase {
    Precheck,
    OwnerPreflight,
    Admission,
    Build,
    Start,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppRepairTarget {
    Project,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppDiagnosticScope {
    Task,
    Service,
}

/// A location is present only when known from typed source information.
/// Messages never contain complete configuration files or environment maps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UserAppDiagnostic {
    /// Stable machine-readable reason; never inferred from localized text.
    pub code: UserAppDiagnosticCode,
    /// Phase where the failure occurred, distinct from overall task kind.
    pub phase: UserAppDiagnosticPhase,
    /// Whether the next repair belongs to project source/configuration or platform/runtime.
    pub repair_target: UserAppRepairTarget,
    /// Task failures use the existing `workspace` log selector; service failures use their real service ID.
    pub scope: UserAppDiagnosticScope,
    /// Configured platform source workspace, when resolution succeeded. This is diagnostic information, not an execution lease.
    pub workspace_root: Option<String>,
    /// Observed misplaced project root; never automatically adopted or migrated.
    pub detected_workspace_root: Option<String>,
    /// Known source manifest path, normally relative to workspace_root.
    pub file: Option<String>,
    /// Known manifest key, such as `run.command`; absent when parsing could not identify a field.
    pub field: Option<String>,
    /// Actual service identity from typed validation/runtime events; absent for task scope.
    pub service_id: Option<String>,
    /// Bounded human-readable cause, without complete configuration or environment values.
    pub message: String,
    /// Bounded repair guidance based on the known failure origin.
    pub hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserAppDiagnosticTaskStatus {
    Failed,
    /// An explicit stop/cancel committed before a worker admission failure.
    Cancelled,
}

/// Admission failure data. A null task_id means retention capacity prevented
/// registration; no non-existent task ID is fabricated.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UserAppTaskFailureData {
    /// Real retained task ID for GET/SSE; null when task retention capacity is exhausted.
    pub task_id: Option<String>,
    /// Failed, or Cancelled if an explicit stop/cancel already committed on the original task.
    pub status: UserAppDiagnosticTaskStatus,
    /// At most 32 structured diagnostics; also retained by task snapshots.
    pub diagnostics: Vec<UserAppDiagnostic>,
    /// Original recovery payload, also preserved at the response data's top level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<serde_json::Value>,
}

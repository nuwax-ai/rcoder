//! Durable userApp control-plane contracts. Runtime identities remain authoritative
//! for physical resources; these records describe application intent and progress.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Canonical request bytes for control-operation digests. Object key order is
/// irrelevant; array order, nulls, and value types remain significant.
pub fn encode_userapp_intent<T: Serialize>(intent: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut value = serde_json::to_value(intent)?;
    value.sort_all_objects();
    serde_json::to_vec(&value)
}

/// Stable reuse identity, distinct from the operation/executor that created it.
pub const USERAPP_RESOURCE_IDENTITY_KEYS: [&str; 3] = [
    "rcoder.io/application-id",
    "rcoder.io/lifecycle-id",
    "rcoder.io/request-fingerprint",
];

/// Identity of an already admitted and claimed control operation. This is an
/// execution credential, not a new request or a lease takeover authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppExecutionContext {
    pub app_id: String,
    pub lifecycle_id: String,
    pub operation_id: String,
    pub executor_id: String,
    pub request_fingerprint: String,
}

impl UserAppExecutionContext {
    /// Runtime-neutral identity metadata (K8s annotations or Docker labels).
    pub fn resource_metadata(&self) -> std::collections::BTreeMap<String, String> {
        [
            ("rcoder.io/application-id", &self.app_id),
            ("rcoder.io/lifecycle-id", &self.lifecycle_id),
            ("rcoder.io/operation-id", &self.operation_id),
            ("rcoder.io/executor-id", &self.executor_id),
            ("rcoder.io/request-fingerprint", &self.request_fingerprint),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.clone()))
        .collect()
    }

    /// Reuse permits a different creation operation in the same lifecycle, but
    /// never another owner, lifecycle, or desired configuration. Missing identity
    /// needs explicit adoption; health alone cannot establish ownership.
    pub fn validate_resource_metadata(
        &self,
        metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        self.validate_application_metadata(metadata)?;
        if metadata.get("rcoder.io/request-fingerprint") != Some(&self.request_fingerprint) {
            return Err("Application resource configuration fingerprint mismatch".into());
        }
        Ok(())
    }

    /// Control operations have their own request digest. They require the same
    /// application, owner and lifecycle, but need not repeat the creation digest.
    /// Physical UID/resourceVersion or container-ID fencing is still required.
    pub fn validate_application_metadata(
        &self,
        metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        self.validate_identity(&self.app_id)?;
        for (key, expected) in [
            ("rcoder.io/application-id", &self.app_id),
            ("rcoder.io/lifecycle-id", &self.lifecycle_id),
        ] {
            if metadata.get(key) != Some(expected) {
                return Err(format!("Application resource identity mismatch: {key}"));
            }
        }
        Ok(())
    }

    pub fn validate_identity(&self, app_id: &str) -> Result<(), String> {
        if self.app_id != app_id {
            return Err("Application execution identity mismatch".into());
        }
        for (name, value) in [
            ("app_id", &self.app_id),
            ("lifecycle_id", &self.lifecycle_id),
            ("operation_id", &self.operation_id),
            ("executor_id", &self.executor_id),
        ] {
            crate::validate_identifier(value, name)
                .map_err(|_| format!("Invalid application execution {name}"))?;
        }
        if self.request_fingerprint.len() != 64
            || !self
                .request_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Invalid application execution fingerprint".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod execution_context_tests {
    use super::UserAppExecutionContext;

    #[test]
    fn control_identity_does_not_reuse_creation_configuration_digest() {
        let creator = UserAppExecutionContext {
            app_id: "app-one".into(),
            lifecycle_id: "life-one".into(),
            operation_id: "create-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut controller = creator.clone();
        controller.operation_id = "stop-one".into();
        controller.executor_id = "executor-two".into();
        controller.request_fingerprint = "b".repeat(64);
        let metadata = creator.resource_metadata();
        controller
            .validate_application_metadata(&metadata)
            .expect("same application lifecycle");
        assert!(
            controller.validate_resource_metadata(&metadata).is_err(),
            "creation reuse still requires matching configuration"
        );
        for key in ["rcoder.io/application-id", "rcoder.io/lifecycle-id"] {
            let mut changed = metadata.clone();
            changed.insert(key.into(), "replacement".into());
            assert!(controller.validate_application_metadata(&changed).is_err());
            changed.remove(key);
            assert!(controller.validate_application_metadata(&changed).is_err());
        }
    }

    #[test]
    fn intent_encoding_sorts_nested_maps_but_preserves_command_order() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"env":{"Z":"z","A":"a"},"command":["run","serve"]}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"command":["run","serve"],"env":{"A":"a","Z":"z"}}"#).unwrap();
        assert_eq!(
            super::encode_userapp_intent(&a).unwrap(),
            super::encode_userapp_intent(&b).unwrap()
        );
        let changed = serde_json::json!({"env":{"A":"a","Z":"z"},"command":["serve","run"]});
        assert_ne!(
            super::encode_userapp_intent(&a).unwrap(),
            super::encode_userapp_intent(&changed).unwrap()
        );
    }

    #[test]
    fn execution_context_requires_exact_ownership_and_nonempty_credentials() {
        let context = UserAppExecutionContext {
            app_id: "app-one".into(),
            lifecycle_id: "life-one".into(),
            operation_id: "op-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "ab".repeat(32),
        };
        assert!(context.validate_identity("app-one").is_ok());
        assert!(context.validate_identity("app-two").is_err());
        let mut invalid = context.clone();
        invalid.operation_id.clear();
        assert!(invalid.validate_identity("app-one").is_err());
        invalid = context;
        invalid.request_fingerprint = "z".repeat(64);
        assert!(invalid.validate_identity("app-one").is_err());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppLifecycleState {
    Active,
    Deleting,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppLifecycleRecord {
    /// Last successfully applied dynamic policy. Omitted values use runtime
    /// creation defaults; they are not inferred from a failed policy operation.
    #[serde(default)]
    pub runtime_policy: UserAppRuntimePolicy,
    pub app_id: String,

    pub lifecycle_id: String,
    pub lifecycle_epoch: i64,
    pub metadata_revision: i64,
    /// Lifecycle state: Active accepts controls, Deleting fences new work while
    /// full deletion runs, and Deleted retains the completed lifecycle tombstone.
    pub state: UserAppLifecycleState,
    pub name: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Per-scope in-flight operation slots. A slot is written by admission of
    /// its scope and cleared only by that operation's terminal commit, so
    /// dev/prod operations never overwrite each other's identity.
    pub active_operations: UserAppActiveOperations,
}

/// Operation-id slots keyed by resource scope. Fixed three-field shape on
/// purpose: extensible string maps would silently accept unknown scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppActiveOperations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
}

impl UserAppActiveOperations {
    pub fn slot(&self, scope: UserAppOperationScope) -> Option<&String> {
        match scope {
            UserAppOperationScope::Dev => self.dev.as_ref(),
            UserAppOperationScope::Prod => self.prod.as_ref(),
            UserAppOperationScope::Application => self.application.as_ref(),
        }
    }
    pub fn slot_mut(&mut self, scope: UserAppOperationScope) -> &mut Option<String> {
        match scope {
            UserAppOperationScope::Dev => &mut self.dev,
            UserAppOperationScope::Prod => &mut self.prod,
            UserAppOperationScope::Application => &mut self.application,
        }
    }
    pub fn set(&mut self, scope: UserAppOperationScope, value: Option<String>) {
        *self.slot_mut(scope) = value;
    }
    pub fn is_empty(&self) -> bool {
        self.dev.is_none() && self.prod.is_none() && self.application.is_none()
    }
    /// Occupied scopes, application first: it fences both environments, so
    /// callers checking blockers observe it before any own-scope slot.
    pub fn occupied_scopes(&self) -> impl Iterator<Item = UserAppOperationScope> + '_ {
        [
            UserAppOperationScope::Application,
            UserAppOperationScope::Dev,
            UserAppOperationScope::Prod,
        ]
        .into_iter()
        .filter(move |scope| self.slot(*scope).is_some())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppOperationState {
    Pending,
    Running,
    WaitingRetry,
    RecoveryRequired,
    Succeeded,
    Failed,
}
impl UserAppOperationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppOperationScope {
    Dev,
    Prod,
    Application,
}

impl UserAppOperationScope {
    pub const ALL: [Self; 3] = [Self::Dev, Self::Prod, Self::Application];

    /// Slot key of the persisted active_operations projection. Fixed vocabulary;
    /// unknown string keys must never deserialize into a scope.
    pub const fn slot_key(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Prod => "prod",
            Self::Application => "application",
        }
    }
}

impl std::fmt::Display for UserAppOperationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dev => "Dev",
            Self::Prod => "Prod",
            Self::Application => "Application",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppOperationKind {
    EnsureBuilder,
    AdoptBuilder,
    StopBuilder,
    RestartBuilder,
    Create,
    StartDeployment,
    RestartDeployment,
    Update,
    Start,
    Restart,
    Stop,
    SetRecyclePolicy,
    HotDeploy,
    DeleteCompute,
    PurgeResources,
    DestroyDevStorage,
    DestroyProdStorage,
    ClearDevStorage,
    ClearProdStorage,
    DeleteApplication,
}
impl UserAppOperationKind {
    pub fn ends_lifecycle(self) -> bool {
        matches!(self, Self::DeleteApplication)
    }

    /// Server-derived resource scope the operation occupies while in flight.
    /// This classification is authoritative: it follows the kind's actual
    /// resource write set and must never be overridden by client input.
    pub fn scope(self) -> UserAppOperationScope {
        match self {
            Self::EnsureBuilder
            | Self::AdoptBuilder
            | Self::StopBuilder
            | Self::RestartBuilder
            | Self::DestroyDevStorage
            | Self::ClearDevStorage => UserAppOperationScope::Dev,
            Self::Create
            | Self::Update
            | Self::StartDeployment
            | Self::RestartDeployment
            | Self::Start
            | Self::Restart
            | Self::Stop
            | Self::SetRecyclePolicy
            | Self::HotDeploy
            | Self::DeleteCompute
            | Self::DestroyProdStorage
            | Self::ClearProdStorage => UserAppOperationScope::Prod,
            Self::PurgeResources | Self::DeleteApplication => UserAppOperationScope::Application,
        }
    }

    /// Whether the operation mutates or fences the shared dev builder itself.
    /// Unrelated in-flight operations (e.g. a production deployment) must not
    /// invalidate a verified builder registration for dev forwarding.
    pub fn affects_builder(self) -> bool {
        self.scope() == UserAppOperationScope::Dev
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UserAppOperationRecord {
    /// Non-secret configuration projection, published atomically with success.
    /// This is not a replayable resource command or proof of remote completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy_on_success: Option<UserAppRuntimePolicy>,
    /// Original non-secret control intent. Missing legacy input is never inferred
    /// from the runtime's current state when deciding whether to replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional tagged command; type is deploy, stop_builder, restart_builder,
    /// create, update, start, restart, stop, set_recycle_policy, delete_resources,
    /// delete_application, destroy_storage, or clear_storage. Private inputs are
    /// referenced by digest and are never embedded in this command.
    pub command: Option<UserAppControlCommand>,
    /// Metadata intent committed atomically with admission; retained for exact retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_metadata: Option<UserAppMetadataPatch>,
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub request_id: Option<String>,
    pub request_fingerprint: String,
    /// Operation kind: EnsureBuilder, AdoptBuilder, StopBuilder, RestartBuilder,
    /// Create, StartDeployment, RestartDeployment, Update, Start, Restart, Stop,
    /// SetRecyclePolicy, HotDeploy, DeleteCompute, PurgeResources, DestroyDevStorage,
    /// DestroyProdStorage, ClearDevStorage, ClearProdStorage, or DeleteApplication.
    pub kind: UserAppOperationKind,
    /// Resource scope this operation occupies. Server-derived from the kind at
    /// admission; never client-supplied, and never defaulted when decoding
    /// persisted records (legacy rows must be migrated, not guessed).
    pub scope: UserAppOperationScope,
    /// Operation state: Pending is unclaimed; Running has an executor;
    /// WaitingRetry awaits a safe retry; RecoveryRequired needs outcome verification;
    /// Succeeded confirms completion; Failed is a terminal failure.
    pub state: UserAppOperationState,
    pub revision: i64,
    /// Execution claim, distinct from the public operation ID.
    #[serde(default)]
    pub executor_id: Option<String>,
    pub step: String,
    /// Non-secret runtime identities and recovery references, never credentials.
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl UserAppOperationRecord {
    /// Structured conflict detail naming this operation as the blocker of an
    /// admission or replay attempt.
    pub fn blocker(&self) -> UserAppOperationBlocker {
        UserAppOperationBlocker {
            scope: self.scope,
            operation_id: self.operation_id.clone(),
            kind: self.kind,
            state: self.state,
            step: self.step.clone(),
        }
    }
}

/// Identity and its linked in-flight operations read from one database
/// snapshot. Useful for reconstruction; it is observational evidence, not a
/// mutation lease.
#[derive(Debug, Clone, PartialEq)]
pub struct UserAppControlSnapshot {
    pub application: UserAppLifecycleRecord,
    pub operations: UserAppActiveOperationRecords,
}

/// Per-scope in-flight operation records aligned with the application's
/// active_operations slots. Slots are never populated with terminal records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserAppActiveOperationRecords {
    pub dev: Option<UserAppOperationRecord>,
    pub prod: Option<UserAppOperationRecord>,
    pub application: Option<UserAppOperationRecord>,
}

impl UserAppActiveOperationRecords {
    pub fn slot(&self, scope: UserAppOperationScope) -> Option<&UserAppOperationRecord> {
        match scope {
            UserAppOperationScope::Dev => self.dev.as_ref(),
            UserAppOperationScope::Prod => self.prod.as_ref(),
            UserAppOperationScope::Application => self.application.as_ref(),
        }
    }
    pub fn set(&mut self, scope: UserAppOperationScope, record: Option<UserAppOperationRecord>) {
        match scope {
            UserAppOperationScope::Dev => self.dev = record,
            UserAppOperationScope::Prod => self.prod = record,
            UserAppOperationScope::Application => self.application = record,
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = &UserAppOperationRecord> {
        [
            self.dev.as_ref(),
            self.prod.as_ref(),
            self.application.as_ref(),
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Debug, Clone)]
pub struct UserAppAdmission {
    /// Configuration operations retain their target policy before runtime effects.
    pub runtime_policy_on_success: Option<UserAppRuntimePolicy>,
    /// Optional tagged command; type is deploy, stop_builder, restart_builder,
    /// create, update, start, restart, stop, set_recycle_policy, delete_resources,
    /// delete_application, destroy_storage, or clear_storage. Private inputs are
    /// referenced by digest and are never embedded in this command.
    pub command: Option<UserAppControlCommand>,
    pub metadata: Option<UserAppMetadataPatch>,
    pub app_id: String,
    pub lifecycle_id: Option<String>,
    pub operation_id: String,
    pub request_id: Option<String>,
    pub request_fingerprint: String,
    /// Operation kind: EnsureBuilder, AdoptBuilder, StopBuilder, RestartBuilder,
    /// Create, StartDeployment, RestartDeployment, Update, Start, Restart, Stop,
    /// SetRecyclePolicy, HotDeploy, DeleteCompute, PurgeResources, DestroyDevStorage,
    /// DestroyProdStorage, ClearDevStorage, ClearProdStorage, or DeleteApplication.
    pub kind: UserAppOperationKind,
}

/// Minimal replayable control inputs, committed before remote mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserAppControlCommand {
    Deploy {
        restart: bool,
        input_digest: String,
    },
    StopBuilder,
    RestartBuilder,
    Create {
        input_digest: String,
    },
    Update {
        input_digest: String,
    },
    Start {
        traffic: bool,
    },
    Restart,
    Stop {
        wake_on_traffic: bool,
    },
    SetRecyclePolicy {
        policy: UserAppRuntimePolicy,
    },
    DeleteResources {
        purge: bool,
        expected_resource_version: Option<String>,
    },
    DeleteApplication,
    DestroyStorage {
        production: bool,
    },
    /// Clear only the selected storage scope; replay is permitted before claim.
    ClearStorage {
        production: bool,
    },
}

impl UserAppControlCommand {
    pub fn kind(&self) -> UserAppOperationKind {
        match self {
            Self::StopBuilder => UserAppOperationKind::StopBuilder,
            Self::RestartBuilder => UserAppOperationKind::RestartBuilder,
            Self::Create { .. } => UserAppOperationKind::Create,
            Self::Deploy { restart: false, .. } => UserAppOperationKind::StartDeployment,
            Self::Deploy { restart: true, .. } => UserAppOperationKind::RestartDeployment,
            Self::Update { .. } => UserAppOperationKind::Update,
            Self::Start { .. } => UserAppOperationKind::Start,
            Self::Restart => UserAppOperationKind::Restart,
            Self::Stop { .. } => UserAppOperationKind::Stop,
            Self::SetRecyclePolicy { .. } => UserAppOperationKind::SetRecyclePolicy,
            Self::DeleteResources { purge: false, .. } => UserAppOperationKind::DeleteCompute,
            Self::DeleteResources { purge: true, .. } => UserAppOperationKind::PurgeResources,
            Self::DeleteApplication => UserAppOperationKind::DeleteApplication,
            Self::DestroyStorage { production: false } => UserAppOperationKind::DestroyDevStorage,
            Self::DestroyStorage { production: true } => UserAppOperationKind::DestroyProdStorage,
            Self::ClearStorage { production: false } => UserAppOperationKind::ClearDevStorage,
            Self::ClearStorage { production: true } => UserAppOperationKind::ClearProdStorage,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppRuntimePolicy {
    pub recycle_enabled: Option<bool>,
    pub idle_timeout_seconds: Option<u64>,
    pub wake_on_traffic: Option<bool>,
}

impl UserAppRuntimePolicy {
    pub fn merge(&self, patch: &Self) -> Self {
        Self {
            recycle_enabled: patch.recycle_enabled.or(self.recycle_enabled),
            idle_timeout_seconds: patch.idle_timeout_seconds.or(self.idle_timeout_seconds),
            wake_on_traffic: patch.wake_on_traffic.or(self.wake_on_traffic),
        }
    }
}

/// Caller identity for controls without a configuration body (for example stop).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppControlRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum UserAppAdmissionOutcome {
    Accepted(UserAppOperationRecord),
    Existing(UserAppOperationRecord),
}

#[derive(Debug, Clone)]
pub struct UserAppOperationProgress {
    pub app_id: String,
    pub operation_id: String,
    pub lifecycle_id: String,
    pub expected_revision: i64,
    pub executor_id: String,
    /// Operation state: Pending is unclaimed; Running has an executor;
    /// WaitingRetry awaits a safe retry; RecoveryRequired needs outcome verification;
    /// Succeeded confirms completion; Failed is a terminal failure.
    pub state: UserAppOperationState,
    pub step: String,
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// None preserves a field; Some(None) explicitly clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppMetadataPatch {
    pub app_id: String,
    pub lifecycle_id: String,
    pub expected_revision: i64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_metadata_field"
    )]
    pub name: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_metadata_field"
    )]
    pub tenant_id: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_metadata_field"
    )]
    pub space_id: Option<Option<String>>,
}

fn deserialize_metadata_field<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

/// A caller's waiting deadline says nothing about the remote operation outcome.
#[derive(Debug, thiserror::Error)]
#[error("Builder ensure deadline exceeded")]
pub struct UserAppWaitTimeout {
    pub operation_id: Option<String>,
}

/// Structured conflict detail about the in-flight operation occupying a scope.
/// Rendered through Display into the existing English conflict message prefix;
/// no internal executor or checkpoint information is exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAppOperationBlocker {
    pub scope: UserAppOperationScope,
    pub operation_id: String,
    pub kind: UserAppOperationKind,
    pub state: UserAppOperationState,
    pub step: String,
}

impl UserAppOperationBlocker {
    fn wire_name<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value)
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned()
    }
}

impl std::fmt::Display for UserAppOperationBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "operation {} ({scope} scope, kind={kind}, state={state}, step={step})",
            self.operation_id,
            scope = self.scope,
            kind = Self::wire_name(&self.kind),
            state = Self::wire_name(&self.state),
            step = self.step,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserAppStoreError {
    #[error("Application ownership conflict")]
    OwnershipConflict,
    #[error("Application lifecycle conflict")]
    LifecycleConflict,
    #[error("Application operation in progress: {0}")]
    OperationInProgress(UserAppOperationBlocker),
    #[error("Application state version conflict")]
    VersionConflict,
    #[error("Application not found")]
    NotFound,
    #[error("Invalid application operation: {0}")]
    InvalidOperation(String),
    #[error("Application storage failed: {0}")]
    Storage(#[source] anyhow::Error),
}

/// All mutating methods commit atomically and return only after commit. A storage
/// error is never equivalent to absence. Implementations must not call runtimes.
#[async_trait::async_trait]
pub trait UserAppLifecycleStore: Send + Sync {
    /// Each page uses a single statement snapshot; rows must not combine a stale
    /// lifecycle pointer with an operation observed after a concurrent commit.
    async fn get_resource_binding(
        &self,
        service_type: &crate::ServiceType,
        physical_uid: &str,
    ) -> Result<Option<crate::UserAppResourceBinding>, UserAppStoreError> {
        let _ = (service_type, physical_uid);
        Err(UserAppStoreError::InvalidOperation(
            "Physical resource bindings are unsupported".into(),
        ))
    }
    /// Atomically commits the physical binding and successful adoption outcome.
    async fn commit_resource_binding(
        &self,
        binding: &crate::UserAppResourceBinding,
        progress: &UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let _ = (binding, progress);
        Err(UserAppStoreError::InvalidOperation(
            "Physical resource bindings are unsupported".into(),
        ))
    }

    async fn list_control_snapshots(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppControlSnapshot>, UserAppStoreError>;

    async fn ensure_identity(
        &self,
        app_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
    async fn get_application(
        &self,
        app_id: &str,
    ) -> Result<Option<UserAppLifecycleRecord>, UserAppStoreError>;
    /// Exclusive app_id cursor. Includes tombstones for migration/reconciliation;
    /// presentation callers must explicitly filter lifecycle state.
    async fn list_applications(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppLifecycleRecord>, UserAppStoreError>;
    /// Import legacy metadata only when no lifecycle exists. Never overwrites a
    /// current lifecycle (including a tombstone), even after a repeated startup.
    async fn import_application(
        &self,
        legacy: &crate::AppMetadataRecord,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
    async fn patch_metadata(
        &self,
        patch: &UserAppMetadataPatch,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
    async fn admit(
        &self,
        request: &UserAppAdmission,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError>;
    /// Private execution input is committed in the same transaction as admission.
    async fn admit_with_input(
        &self,
        request: &UserAppAdmission,
        input: Option<&UserAppExecutionInput>,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError> {
        if input.is_some() {
            return Err(UserAppStoreError::InvalidOperation(
                "Private execution inputs are unsupported".into(),
            ));
        }
        self.admit(request).await
    }
    /// Available only to the executor that owns the current lifecycle operation.
    async fn read_execution_input(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<UserAppExecutionInput, UserAppStoreError> {
        let _ = context;
        Err(UserAppStoreError::InvalidOperation(
            "Private execution inputs are unsupported".into(),
        ))
    }
    /// 绑定操作执行 deadline（epoch ms，旁记录——不动 DeployInput；bind-once：
    /// 已存在时返回持久化值不覆盖，不同副本配置不同也不得各绑各的候选值）。
    /// 与受理原子写入（或至少在任何运行时副作用之前完成；失败 fail-closed）。
    async fn bind_operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
        lifecycle_id: &str,
        deadline_epoch_ms: i64,
    ) -> Result<i64, UserAppStoreError>;
    /// 读取操作绑定的 deadline（None = 无记录：旧版本受理的操作，由调用方
    /// 按 created_at + absolute_budget 推导并 bind-once 落盘）。
    async fn operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<i64>, UserAppStoreError>;
    async fn bind_operation_lease(
        &self,
        context: &UserAppExecutionContext,
        receipt: &crate::UserAppOperationLeaseReceipt,
    ) -> Result<(), UserAppStoreError> {
        let _ = (context, receipt);
        Err(UserAppStoreError::InvalidOperation(
            "Durable operation lease binding is unsupported".into(),
        ))
    }
    async fn get_operation_lease(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<crate::UserAppOperationLeaseBinding>, UserAppStoreError> {
        let _ = (app_id, operation_id);
        Err(UserAppStoreError::InvalidOperation(
            "Durable operation lease binding is unsupported".into(),
        ))
    }
    /// Bounded page of terminal operations whose exact mutex receipt still needs cleanup.
    async fn terminal_operation_leases(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<crate::UserAppOperationLeaseBinding>, UserAppStoreError> {
        let _ = (after, limit);
        Err(UserAppStoreError::InvalidOperation(
            "Terminal lease discovery is unsupported".into(),
        ))
    }
    /// Forget a receipt only after conditional runtime release and a terminal SQL result.
    async fn forget_operation_lease(
        &self,
        binding: &crate::UserAppOperationLeaseBinding,
    ) -> Result<(), UserAppStoreError> {
        let _ = binding;
        Err(UserAppStoreError::InvalidOperation(
            "Terminal lease cleanup is unsupported".into(),
        ))
    }
    /// Reserve only a confirmed final checkpoint. This authorizes mutex cleanup,
    /// never replay of application writes or adoption of an unfinished executor.
    async fn reserve_completed_operation(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> Result<UserAppOperationRecord, UserAppStoreError> {
        let _ = snapshot;
        Err(UserAppStoreError::InvalidOperation(
            "Completed operation recovery is unsupported".into(),
        ))
    }
    async fn advance(
        &self,
        progress: &UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, UserAppStoreError>;
    /// Resolve an admitted caller token, including tokens joined to an operation.
    async fn get_operation_by_request(
        &self,
        app_id: &str,
        request_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, UserAppStoreError>;
    async fn get_operation(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, UserAppStoreError>;
    async fn unfinished_operations(
        &self,
        after_operation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppOperationRecord>, UserAppStoreError>;
    async fn recreate(
        &self,
        app_id: &str,
        expected_lifecycle_id: &str,
        request_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
}

/// A runtime operation lease is occupied. This is distinct from a resource CAS
/// conflict. A missing operation_id denotes a legacy/unidentified lease and is
/// never sufficient evidence to join, release, or replay the operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[error(
    "Application operation in progress: {resource_name} (app={app_id}, operation={operation_id:?})"
)]
pub struct UserAppOperationInProgress {
    pub app_id: String,
    /// Resource family. Wire values are web-agent-runner (WebAgentRunner),
    /// computer-agent-runner (ComputerAgentRunner), user-app (Userapp), and
    /// user-app-builder (UserappBuilder); application leases use the last two only.
    pub service_type: crate::ServiceType,
    pub resource_name: String,
    pub operation_id: Option<String>,
}

/// Public operation projection. Executor identities and internal recovery
/// checkpoints are intentionally excluded from the HTTP contract.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppOperationView {
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub request_id: Option<String>,
    /// Operation kind: EnsureBuilder, AdoptBuilder, StopBuilder, RestartBuilder,
    /// Create, StartDeployment, RestartDeployment, Update, Start, Restart, Stop,
    /// SetRecyclePolicy, HotDeploy, DeleteCompute, PurgeResources, DestroyDevStorage,
    /// DestroyProdStorage, ClearDevStorage, ClearProdStorage, or DeleteApplication.
    pub kind: UserAppOperationKind,
    /// Resource scope this operation occupies: Dev (builder), Prod (production
    /// runtime) or Application (both environments plus shared authority).
    pub scope: UserAppOperationScope,
    /// Operation state: Pending is unclaimed; Running has an executor;
    /// WaitingRetry awaits a safe retry; RecoveryRequired needs outcome verification;
    /// Succeeded confirms completion; Failed is a terminal failure.
    pub state: UserAppOperationState,
    pub revision: i64,
    pub step: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
impl From<UserAppOperationRecord> for UserAppOperationView {
    fn from(record: UserAppOperationRecord) -> Self {
        Self {
            operation_id: record.operation_id,
            app_id: record.app_id,
            lifecycle_id: record.lifecycle_id,
            request_id: record.request_id,
            kind: record.kind,
            scope: record.scope,
            state: record.state,
            revision: record.revision,
            step: record.step,
            error_code: record.error_code,
            error_message: record.error_message,
            created_at: record.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct UserAppRetryRequest {
    #[garde(length(min = 1, max = 128))]
    pub lifecycle_id: String,
    #[garde(range(min = 1))]
    pub expected_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct UserAppRecreateRequest {
    #[garde(length(min = 1, max = 128))]
    pub expected_lifecycle_id: String,
    #[garde(length(min = 1, max = 128))]
    pub request_id: String,
}

/// Opaque internal input. Intentionally has no Serialize/ToSchema implementation;
/// neither operation queries nor diagnostics may expose credentials it contains.
#[derive(Clone)]
pub struct UserAppExecutionInput(String);
impl std::fmt::Debug for UserAppExecutionInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UserAppExecutionInput([REDACTED])")
    }
}
impl UserAppExecutionInput {
    pub fn new(encoded: String) -> Self {
        Self(encoded)
    }
    pub fn encoded(&self) -> &str {
        &self.0
    }
    pub fn digest(&self) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(self.0.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod operation_scope_tests {
    use super::{
        UserAppOperationBlocker, UserAppOperationKind as Kind, UserAppOperationScope as Scope,
        UserAppOperationState as State,
    };

    const DEV: [Kind; 6] = [
        Kind::EnsureBuilder,
        Kind::AdoptBuilder,
        Kind::StopBuilder,
        Kind::RestartBuilder,
        Kind::DestroyDevStorage,
        Kind::ClearDevStorage,
    ];
    const PROD: [Kind; 12] = [
        Kind::Create,
        Kind::Update,
        Kind::StartDeployment,
        Kind::RestartDeployment,
        Kind::Start,
        Kind::Restart,
        Kind::Stop,
        Kind::SetRecyclePolicy,
        Kind::HotDeploy,
        Kind::DeleteCompute,
        Kind::DestroyProdStorage,
        Kind::ClearProdStorage,
    ];
    const APPLICATION: [Kind; 2] = [Kind::PurgeResources, Kind::DeleteApplication];

    #[test]
    fn every_kind_maps_to_the_audited_resource_scope() {
        for kind in DEV {
            assert_eq!(kind.scope(), Scope::Dev, "{kind:?}");
        }
        for kind in PROD {
            assert_eq!(kind.scope(), Scope::Prod, "{kind:?}");
        }
        for kind in APPLICATION {
            assert_eq!(kind.scope(), Scope::Application, "{kind:?}");
        }
        let total = DEV.len() + PROD.len() + APPLICATION.len();
        assert_eq!(total, 20, "scope matrix must stay exhaustive");
    }

    #[test]
    fn affects_builder_delegates_to_dev_scope() {
        for kind in DEV {
            assert!(kind.affects_builder(), "{kind:?}");
        }
        for kind in PROD.iter().chain(APPLICATION.iter()) {
            assert!(!kind.affects_builder(), "{kind:?}");
        }
    }

    #[test]
    fn scope_slot_keys_use_fixed_vocabulary() {
        assert_eq!(Scope::Dev.slot_key(), "dev");
        assert_eq!(Scope::Prod.slot_key(), "prod");
        assert_eq!(Scope::Application.slot_key(), "application");
    }

    #[test]
    fn blocker_display_keeps_operation_id_and_english_detail() {
        let blocker = UserAppOperationBlocker {
            scope: Scope::Prod,
            operation_id: "op-42".into(),
            kind: Kind::Start,
            state: State::RecoveryRequired,
            step: "claimed".into(),
        };
        let text = blocker.to_string();
        assert!(text.contains("op-42"), "{text}");
        assert!(text.contains("Prod"), "{text}");
        assert!(text.contains("kind=Start"), "{text}");
        assert!(text.contains("state=RecoveryRequired"), "{text}");
        assert!(text.contains("step=claimed"), "{text}");
    }
}

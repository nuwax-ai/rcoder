//! Storage-only Toasty models. Public lifecycle DTOs are reconstructed explicitly;
//! the database never keeps a second authoritative whole-record JSON snapshot.

#[derive(toasty::Model)]
#[table = "userapps"]
pub(crate) struct Application {
    #[key]
    pub app_id: String,
    pub lifecycle_id: String,
    pub lifecycle_epoch: i64,
    pub lifecycle_state: String,
    pub metadata_revision: i64,
    pub name: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub recycle_enabled: Option<bool>,
    pub wake_on_traffic: Option<bool>,
    pub idle_timeout_seconds: Option<i64>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_operations"]
pub(crate) struct Operation {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub kind: String,
    pub scope: String,
    pub state: String,
    pub revision: i64,
    pub origin_request_id: Option<String>,
    pub request_fingerprint: String,
    pub executor_id: Option<String>,
    pub step: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub payload_version: i64,
    pub command_json: Option<String>,
    pub admitted_metadata_json: Option<String>,
    pub runtime_policy_on_success_json: Option<String>,
    pub checkpoint_json: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
    pub terminal_at_us: Option<i64>,
}

#[derive(toasty::Model)]
#[table = "userapp_active_operations"]
pub(crate) struct ActiveOperations {
    #[key]
    pub app_id: String,
    pub lifecycle_id: String,
    pub dev_operation_id: Option<String>,
    pub prod_operation_id: Option<String>,
    pub application_operation_id: Option<String>,
}

#[derive(toasty::Model)]
#[table = "userapp_requests"]
#[key(app_id, request_id)]
pub(crate) struct Request {
    pub app_id: String,
    pub request_id: String,
    pub target_kind: String,
    pub operation_id: Option<String>,
    pub lifecycle_id: Option<String>,
    pub previous_lifecycle_id: Option<String>,
    pub new_lifecycle_id: Option<String>,
    pub created_at_us: i64,
}

// Private models deliberately do not derive Debug or Serialize.
#[derive(toasty::Model)]
#[table = "userapp_operation_inputs"]
pub(crate) struct OperationInput {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub payload_version: i64,
    pub payload: String,
    pub payload_digest: String,
    pub created_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_operation_leases"]
pub(crate) struct OperationLease {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub executor_id: String,
    pub request_fingerprint: String,
    pub receipt_version: i64,
    pub receipt_json: String,
    pub created_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_operation_deadlines"]
pub(crate) struct OperationDeadline {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub deadline_ms: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_resource_bindings"]
#[key(service_type, physical_uid)]
pub(crate) struct ResourceBinding {
    pub service_type: String,
    pub physical_uid: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub adopted_by_operation: String,
    pub created_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_recovery_witnesses"]
#[key(app_id, lifecycle_id)]
pub(crate) struct RecoveryWitness {
    pub app_id: String,
    pub lifecycle_id: String,
    pub witness_json: String,
    pub created_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_activity"]
#[key(app_id, lifecycle_id, scope)]
pub(crate) struct Activity {
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: String,
    pub last_accessed_at_us: i64,
    pub updated_at_us: i64,
}

/// Immutable configuration payloads: a concurrent save must not overwrite the
/// version already captured by an admitted startup operation.
#[derive(toasty::Model)]
#[table = "userapp_runtime_config_versions"]
#[key(app_id, lifecycle_id, scope, version)]
pub(crate) struct RuntimeConfigVersion {
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: String,
    pub version: i64,
    pub request_id: String,
    pub expected_revision: i64,
    pub payload_version: i64,
    pub pg_username: String,
    pub pg_password: String,
    pub created_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_runtime_configs"]
#[key(app_id, lifecycle_id, scope)]
pub(crate) struct RuntimeConfig {
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: String,
    pub revision: i64,
    pub saved_version: i64,
    pub applied_version: Option<i64>,
    pub applying_version: Option<i64>,
    pub applying_operation_id: Option<String>,
    pub updated_at_us: i64,
}

/// Every execution reads its immutable capture, including recovery. The physical
/// identity is bound before credential-changing side effects and never replaced
/// merely because a newer pod/container appears under the same name.
#[derive(toasty::Model)]
#[table = "userapp_operation_configs"]
pub(crate) struct OperationConfig {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: String,
    pub config_version: i64,
    pub physical_uid: Option<String>,
    pub deployment_generation: Option<String>,
    pub credential_state: String,
    pub business_state: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

pub(crate) fn storage_models() -> toasty::ModelSet {
    toasty::models!(
        Application,
        ComputeIntent,
        ComputeControl,
        Operation,
        ActiveOperations,
        Request,
        OperationInput,
        OperationLease,
        OperationDeadline,
        ResourceBinding,
        RecoveryWitness,
        Activity,
        RuntimeConfigVersion,
        RuntimeConfig,
        OperationConfig,
        PreviewInstance,
        PreviewOperation,
        Container,
        Project,
        Session,
        ProjectTombstone,
        SessionTombstone,
        ContainerTombstone,
        ProjectWriteReceipt
    )
}

#[derive(toasty::Model)]
#[table = "preview_instances"]
pub(crate) struct PreviewInstance {
    #[key]
    pub preview_key: String,
    pub project_id: String,
    pub project_path: String,
    pub instance_id: String,
    pub revision: i64,
    pub operation_id: Option<String>,
    pub host_id: String,
    pub pod_name: Option<String>,
    pub pod_ip: Option<String>,
    pub pid: Option<i64>,
    pub port: Option<i64>,
    pub base_path: Option<String>,
    pub state: String,
    pub last_heartbeat_at_us: Option<i64>,
    pub last_activity_at_us: i64,
    pub detail: Option<String>,
    pub updated_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "preview_operations"]
pub(crate) struct PreviewOperation {
    #[key]
    pub operation_id: String,
    pub preview_key: String,
    pub instance_id: String,
    pub request_fingerprint: String,
    pub kind: String,
    pub state: String,
    pub host_id: String,
    pub requested_port: Option<i64>,
    pub allocated_port: Option<i64>,
    pub payload_version: i64,
    pub request_json: String,
    pub result_json: Option<String>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "containers"]
pub(crate) struct Container {
    #[key]
    pub container_name: String,
    pub container_generation: String,
    pub container_id: Option<String>,
    /// §1.1 持久 workload 身份（K8s 控制器 UID；Docker 恒 NULL）。
    pub workload_uid: Option<String>,
    pub logical_id: String,
    pub service_type: String,
    pub container_ip: String,
    pub internal_port: i64,
    pub external_port: i64,
    pub status: String,
    pub service_url: String,
    pub last_activity_at_us: i64,
    pub created_at_us: i64,
    pub row_revision: i64,
}

#[derive(toasty::Model)]
#[table = "projects"]
pub(crate) struct Project {
    #[key]
    pub project_id: String,
    pub generation: String,
    pub user_id: Option<String>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    pub container_name: Option<String>,
    pub container_generation: Option<String>,
    pub latest_session: Option<String>,
    pub model_provider_json: Option<String>,
    pub request_id: Option<String>,
    pub agent_status_json: Option<String>,
    pub service_type: Option<String>,
    pub payload_version: i64,
    pub last_activity_at_us: i64,
    pub created_at_us: i64,
    pub row_revision: i64,
}

#[derive(toasty::Model)]
#[table = "sessions"]
pub(crate) struct Session {
    #[key]
    pub session_id: String,
    pub generation: String,
    pub project_id: String,
    pub project_generation: String,
    pub created_at_us: i64,
    pub last_seen_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "project_tombstones"]
#[key(project_id, generation)]
pub(crate) struct ProjectTombstone {
    pub project_id: String,
    pub generation: String,
    pub retired_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "session_tombstones"]
#[key(session_id, generation)]
pub(crate) struct SessionTombstone {
    pub session_id: String,
    pub generation: String,
    pub retired_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "container_tombstones"]
#[key(container_name, container_generation)]
pub(crate) struct ContainerTombstone {
    pub container_name: String,
    pub container_generation: String,
    pub physical_uid: Option<String>,
    pub retired_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "project_write_receipts"]
pub(crate) struct ProjectWriteReceipt {
    #[key]
    pub request_id: String,
    pub fingerprint: String,
    pub outcome: String,
    pub recorded_at_us: i64,
}

#[derive(toasty::Model)]
#[table = "userapp_compute_intents"]
#[key(app_id, scope)]
pub(crate) struct ComputeIntent {
    pub app_id: String,
    pub scope: String,
    pub lifecycle_id: String,
    pub generation: i64,
    pub revision: i64,
    pub desired_state: String,
    pub control_operation_id: Option<String>,
    pub updated_at_us: i64,
}
#[derive(toasty::Model)]
#[table = "userapp_compute_controls"]
pub(crate) struct ComputeControl {
    #[key]
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: String,
    pub generation: i64,
    pub revision: i64,
    pub action: String,
    pub state: String,
    pub request_id: String,
    pub request_fingerprint: String,
    pub executor_id: Option<String>,
    pub stage: String,
    pub evidence_json: String,
    pub checkpoint_json: String,
    pub lease_json: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
    pub terminal_at_us: Option<i64>,
}

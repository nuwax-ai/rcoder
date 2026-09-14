//! 行映射与纯转换（无 I/O）。状态串与枚举的往返在此收敛。
use shared_types::{
    PreviewInstanceRecord, PreviewInstanceState, PreviewOperationKind, PreviewOperationRecord,
    PreviewOperationState, PreviewStoreError,
};

#[derive(Debug, sqlx::FromRow)]
pub(super) struct PreviewRow {
    pub preview_key: String,
    pub project_id: String,
    pub project_path: String,
    pub instance_id: String,
    pub revision: i64,
    pub operation_id: String,
    pub host_id: String,
    pub pod_name: Option<String>,
    pub pod_ip: Option<String>,
    pub pid: Option<i64>,
    pub port: Option<i32>,
    pub base_path: Option<String>,
    pub state: String,
    pub last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
    pub detail: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub(super) fn parse_state(raw: &str) -> Result<PreviewInstanceState, PreviewStoreError> {
    match raw {
        "starting" => Ok(PreviewInstanceState::Starting),
        "ready" => Ok(PreviewInstanceState::Ready),
        "stopping" => Ok(PreviewInstanceState::Stopping),
        "stopped" => Ok(PreviewInstanceState::Stopped),
        "failed" => Ok(PreviewInstanceState::Failed),
        "unknown" => Ok(PreviewInstanceState::Unknown),
        other => Err(PreviewStoreError::Unavailable(format!(
            "corrupt preview state value: {other}"
        ))),
    }
}

pub(super) fn state_str(state: PreviewInstanceState) -> &'static str {
    match state {
        PreviewInstanceState::Starting => "starting",
        PreviewInstanceState::Ready => "ready",
        PreviewInstanceState::Stopping => "stopping",
        PreviewInstanceState::Stopped => "stopped",
        PreviewInstanceState::Failed => "failed",
        PreviewInstanceState::Unknown => "unknown",
    }
}

impl PreviewRow {
    pub(super) fn to_record(&self) -> Result<PreviewInstanceRecord, PreviewStoreError> {
        Ok(PreviewInstanceRecord {
            preview_key: self.preview_key.clone(),
            project_id: self.project_id.clone(),
            project_path: self.project_path.clone(),
            instance_id: self.instance_id.clone(),
            revision: self.revision,
            operation_id: self.operation_id.clone(),
            host_id: self.host_id.clone(),
            pod_name: self.pod_name.clone(),
            pod_ip: self.pod_ip.clone(),
            pid: self.pid,
            port: self.port.and_then(|p| u16::try_from(p).ok()),
            base_path: self.base_path.clone(),
            state: parse_state(&self.state)?,
            last_heartbeat_at: self.last_heartbeat_at,
            last_activity_at: self.last_activity_at,
            detail: self.detail.clone(),
            updated_at: self.updated_at,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct OperationRow {
    pub operation_id: String,
    pub preview_key: String,
    pub kind: String,
    pub state: String,
    pub host_id: String,
    pub requested_port: Option<i32>,
    pub allocated_port: Option<i32>,
    pub result: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl OperationRow {
    #[allow(dead_code)] // 审计读取入口保留给后续诊断接口
    pub(super) fn to_record(&self) -> Result<PreviewOperationRecord, PreviewStoreError> {
        let kind = match self.kind.as_str() {
            "start" => PreviewOperationKind::Start,
            "stop" => PreviewOperationKind::Stop,
            other => {
                return Err(PreviewStoreError::Unavailable(format!(
                    "corrupt preview operation kind: {other}"
                )));
            }
        };
        let state = match self.state.as_str() {
            "accepted" => PreviewOperationState::Accepted,
            "running" => PreviewOperationState::Running,
            "succeeded" => PreviewOperationState::Succeeded,
            "failed" => PreviewOperationState::Failed,
            "uncertain" => PreviewOperationState::Uncertain,
            other => {
                return Err(PreviewStoreError::Unavailable(format!(
                    "corrupt preview operation state: {other}"
                )));
            }
        };
        Ok(PreviewOperationRecord {
            operation_id: self.operation_id.clone(),
            preview_key: self.preview_key.clone(),
            kind,
            state,
            host_id: self.host_id.clone(),
            requested_port: self.requested_port.and_then(|p| u16::try_from(p).ok()),
            allocated_port: self.allocated_port.and_then(|p| u16::try_from(p).ok()),
            result: self.result.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

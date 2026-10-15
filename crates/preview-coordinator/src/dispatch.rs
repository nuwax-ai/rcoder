//! 跨 Pod 派发客户端（协调器 → 宿主 Pod 的 preview-internal 端点）。
//!
//! 仅限协调器主动派发（stop/verify/log）；目标地址来自权威库行的 pod_ip，
//! 不接受外部输入指定。令牌仅进程间持有，外部请求伪造无效（对端校验）。
use shared_types::{ExecutorLogChunk, ExecutorLogLine, ExecutorStopOutcome, ExecutorVerifyReport};

#[derive(Debug, thiserror::Error)]
#[error("remote preview host unreachable: {0}")]
pub struct RemoteDispatchError(String);

impl RemoteDispatchError {
    fn new(context: &str) -> Self {
        Self(context.to_string())
    }
}

pub struct RemoteDispatch {
    client: reqwest::Client,
    token: String,
    peer_port: u16,
}

#[derive(serde::Deserialize)]
struct StopResponse {
    outcome: String,
}

#[derive(serde::Deserialize)]
struct VerifyResponse {
    identity_match: bool,
    alive: bool,
    pid: Option<i64>,
    port: Option<u16>,
}

impl RemoteDispatch {
    pub fn new(token: String, peer_port: u16, dispatch_timeout_secs: u64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(dispatch_timeout_secs))
            .build()
            .unwrap_or_default();
        Self {
            client,
            token,
            peer_port,
        }
    }

    fn url(&self, pod_ip: &str, path: &str) -> String {
        format!("http://{pod_ip}:{}/{path}", self.peer_port)
    }

    /// 远端执行 stop（宿主侧校验操作身份后按登记 pid 组杀）。
    pub async fn remote_stop(
        &self,
        pod_ip: &str,
        preview_key: &str,
        instance_id: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<ExecutorStopOutcome, RemoteDispatchError> {
        let response = self
            .client
            .post(self.url(pod_ip, "api/v1/preview-internal/stop"))
            .header("x-preview-internal-token", &self.token)
            .json(&serde_json::json!({
                "previewKey": preview_key,
                "instanceId": instance_id,
                "operationId": operation_id,
                "revision": revision,
            }))
            .send()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("stop dispatch: {e}")))?;
        if !response.status().is_success() {
            return Err(RemoteDispatchError::new(&format!(
                "stop dispatch status {}",
                response.status()
            )));
        }
        let body: StopResponse = response
            .json()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("stop decode: {e}")))?;
        match body.outcome.as_str() {
            "stopped" => Ok(ExecutorStopOutcome::Stopped),
            "not_registered" => Ok(ExecutorStopOutcome::NotRegistered),
            "identity_mismatch" => Ok(ExecutorStopOutcome::IdentityMismatch),
            other => Err(RemoteDispatchError::new(&format!(
                "unknown stop outcome {other}"
            ))),
        }
    }

    /// 远端 verify（存活判定；短超时）。
    pub async fn remote_verify(
        &self,
        pod_ip: &str,
        preview_key: &str,
        instance_id: &str,
        verify_timeout_secs: u64,
    ) -> Result<ExecutorVerifyReport, RemoteDispatchError> {
        let response = self
            .client
            .post(self.url(pod_ip, "api/v1/preview-internal/verify"))
            .header("x-preview-internal-token", &self.token)
            .timeout(std::time::Duration::from_secs(verify_timeout_secs))
            .json(&serde_json::json!({
                "previewKey": preview_key,
                "instanceId": instance_id,
            }))
            .send()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("verify dispatch: {e}")))?;
        if !response.status().is_success() {
            return Err(RemoteDispatchError::new(&format!(
                "verify dispatch status {}",
                response.status()
            )));
        }
        let body: VerifyResponse = response
            .json()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("verify decode: {e}")))?;
        Ok(ExecutorVerifyReport {
            identity_match: body.identity_match,
            alive: body.alive,
            pid: body.pid,
            port: body.port,
        })
    }

    /// 远端读日志（get-dev-log 转发源）。
    pub async fn remote_log(
        &self,
        pod_ip: &str,
        log_key: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, RemoteDispatchError> {
        let response = self
            .client
            .get(self.url(pod_ip, "api/v1/preview-internal/log"))
            .header("x-preview-internal-token", &self.token)
            .query(&[
                ("logKey", log_key.to_string()),
                ("logType", log_type.to_string()),
                ("startIndex", start_index.to_string()),
            ])
            .send()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("log dispatch: {e}")))?;
        if !response.status().is_success() {
            return Err(RemoteDispatchError::new(&format!(
                "log dispatch status {}",
                response.status()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| RemoteDispatchError::new(&format!("log decode: {e}")))?;
        let logs = body
            .get("logs")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<ExecutorLogLine>>(v).ok())
            .unwrap_or_default();
        Ok(ExecutorLogChunk {
            logs,
            total_lines: body.get("totalLines").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            log_file_name: body
                .get("logFileName")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }
}

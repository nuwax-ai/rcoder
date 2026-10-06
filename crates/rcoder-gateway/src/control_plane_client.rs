//! rcoder-control 内部 HTTP 客户端。服务凭据来自配置，错误保持原诊断和身份。

use shared_types::{AppError, ErrorDetail, error_codes};

#[derive(Clone)]
pub struct ControlPlaneClient {
    base_url: String,
    client: reqwest::Client,
    api_key: Option<String>,
    request_timeout: std::time::Duration,
}

#[derive(Debug, serde::Deserialize)]
pub struct EnsurePodResponse {
    pub success: bool,
    pub data: Option<EnsurePodData>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsurePodData {
    #[serde(default)]
    pub container_name: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct SessionResolveResponse {
    pub success: bool,
    pub data: Option<SessionResolveData>,
    pub message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResolveData {
    pub identifier: String,
    pub service_type: String,
}

impl ControlPlaneClient {
    pub fn new(base_url: String) -> Self {
        Self::with_configured_key(base_url, None)
    }

    pub fn with_configured_key(base_url: String, api_key: Option<String>) -> Self {
        Self {
            base_url,
            client: reqwest::Client::new(),
            api_key,
            request_timeout: std::time::Duration::from_secs(30),
        }
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.timeout(self.request_timeout);
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    pub async fn ensure_pod(
        &self,
        identifier: &str,
        service_type: &str,
    ) -> Result<EnsurePodResponse, AppError> {
        let request = self
            .client
            .post(format!("{}/internal/pod/ensure", self.base_url))
            .json(&serde_json::json!({"identifier": identifier, "service_type": service_type}));
        self.send(request, "gateway_ensure", true).await
    }

    pub async fn resolve_session(
        &self,
        session_id: &str,
    ) -> Result<SessionResolveResponse, AppError> {
        let url = format!("{}/internal/session/{session_id}/resolve", self.base_url);
        self.send(self.client.get(url), "gateway_session_resolve", false)
            .await
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        stage: &'static str,
        mutation: bool,
    ) -> Result<T, AppError> {
        let response = self.authenticated(request).send().await.map_err(|error| {
            let cause = if error.is_builder() {
                error_codes::ERR_RUNTIME_CONFIGURATION
            } else if error.is_timeout() {
                error_codes::ERR_RUNTIME_TIMEOUT
            } else {
                error_codes::ERR_RUNTIME_UNAVAILABLE
            };
            let unknown = mutation && !error.is_builder() && !error.is_connect();
            diagnostic(
                if unknown { error_codes::ERR_OPERATION_OUTCOME_UNKNOWN } else { cause },
                cause, stage,
                "The control-plane request did not complete; check the configured address and service availability",
                !mutation && !error.is_builder(),
            )
        })?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.map_err(|error| {
            let cause = if error.is_timeout() {
                error_codes::ERR_RUNTIME_TIMEOUT
            } else {
                error_codes::ERR_RUNTIME_UNAVAILABLE
            };
            diagnostic(
                if mutation {
                    error_codes::ERR_OPERATION_OUTCOME_UNKNOWN
                } else {
                    cause
                },
                cause,
                &format!("{stage}_response_body"),
                if error.is_timeout() {
                    "The control-plane response body did not complete before the request deadline"
                } else {
                    "The control plane returned an incomplete or invalid JSON response"
                },
                !mutation,
            )
        })?;
        if !status.is_success() || body["success"].as_bool() != Some(true) {
            return Err(response_error(status.as_u16(), &body, stage));
        }
        serde_json::from_value(body).map_err(|_| {
            diagnostic(
                error_codes::ERR_CONTAINER_ADDRESS_NOT_READY,
                error_codes::ERR_CONTAINER_ADDRESS_NOT_READY,
                stage,
                "The control-plane response is missing required routing data",
                false,
            )
        })
    }
}

pub(crate) fn diagnostic(
    code: &str,
    cause: &str,
    stage: &str,
    message: &str,
    retryable: bool,
) -> AppError {
    AppError::with_message(code, message)
        .with_error_detail(ErrorDetail::new(cause, stage, message).with_retryable(retryable))
}

fn response_error(status: u16, body: &serde_json::Value, stage: &str) -> AppError {
    let code = body["code"]
        .as_str()
        .filter(|code| !code.is_empty() && *code != "0000")
        .unwrap_or(match status {
            503 => error_codes::ERR_RUNTIME_UNAVAILABLE,
            504 => error_codes::ERR_RUNTIME_TIMEOUT,
            401 | 403 => error_codes::ERR_API_KEY_AUTH_FAILED,
            _ => error_codes::ERR_CONTAINER_START_FAILED,
        });
    let message = body["message"]
        .as_str()
        .unwrap_or("The control-plane request failed");
    let mut error = diagnostic(code, code, stage, message, false);
    if let Some(detail) = body
        .get("error_detail")
        .and_then(|value| serde_json::from_value::<ErrorDetail>(value.clone()).ok())
    {
        error = error.with_error_detail(detail);
    }
    if let Some(operation_id) = body["operation_id"].as_str() {
        error = error.with_operation_id(operation_id.to_owned());
    }
    if let Some(blocker) = body.get("blocker").and_then(|value| {
        serde_json::from_value::<shared_types::UserAppOperationBlocker>(value.clone()).ok()
    }) {
        error = error.with_blocker(blocker);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_error_preserves_original_identity_and_detail() {
        let error = response_error(200, &serde_json::json!({
            "success":false, "code":"ERR_RUNTIME_TIMEOUT", "message":"runtime timed out",
            "operation_id":"original-operation", "error_detail":{
                "reason_code":"ERR_RUNTIME_TIMEOUT", "stage":"runtime_query", "detail":"query timed out",
                "hint":"check runtime", "retryable":false
            }
        }), "gateway_ensure").into_http_result::<()>("en-US");
        assert_eq!(error.code, "ERR_RUNTIME_TIMEOUT");
        assert_eq!(error.operation_id.as_deref(), Some("original-operation"));
        assert_eq!(error.error_detail.expect("detail").stage, "runtime_query");
    }
    #[tokio::test]
    async fn configured_service_key_is_sent_on_ensure_and_resolve() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let length = socket.read(&mut chunk).await.expect("read request");
                    if length == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..length]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let body_length = header
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + body_length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                assert!(
                    request
                        .lines()
                        .any(|line| line.eq_ignore_ascii_case("x-api-key: fixture-service-key"))
                );
                let body = if request.starts_with("POST") {
                    r#"{"success":true,"data":{"containerName":"rcoder-web-app"}}"#
                } else {
                    r#"{"success":true,"data":{"identifier":"app","serviceType":"web-agent-runner"}}"#
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(reply.as_bytes()).await.expect("response");
            }
        });
        let client =
            ControlPlaneClient::with_configured_key(base, Some("fixture-service-key".into()));
        client
            .ensure_pod("app", "web-agent-runner")
            .await
            .expect("ensure");
        client.resolve_session("session").await.expect("resolve");
        server.await.expect("fixture finished");
    }
    #[tokio::test]
    async fn response_body_timeout_keeps_read_cause_and_protects_dispatched_write() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for mutation in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture");
            let base = format!("http://{}", listener.local_addr().expect("address"));
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut input = Vec::new();
                loop {
                    let mut bytes = [0; 4096];
                    let length = socket.read(&mut bytes).await.expect("request");
                    input.extend_from_slice(&bytes[..length]);
                    if length == 0 {
                        break;
                    }
                    if let Some(end) = input.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]);
                        let body_length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + body_length {
                            break;
                        }
                    }
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"success\":true,").await.expect("partial response");
                stopped.await.expect("release fixture");
            });
            let mut client = ControlPlaneClient::new(base);
            client.request_timeout = std::time::Duration::from_millis(100);
            let error = if mutation {
                client
                    .ensure_pod("app", "web-agent-runner")
                    .await
                    .unwrap_err()
            } else {
                client.resolve_session("session").await.unwrap_err()
            };
            stop.send(()).expect("stop fixture");
            server.await.expect("join fixture");
            let response = error.into_http_result::<()>("en-US");
            assert_eq!(
                response.code,
                if mutation {
                    error_codes::ERR_OPERATION_OUTCOME_UNKNOWN
                } else {
                    error_codes::ERR_RUNTIME_TIMEOUT
                }
            );
            let detail = response.error_detail.expect("detail");
            assert_eq!(detail.reason_code, error_codes::ERR_RUNTIME_TIMEOUT);
            assert_eq!(
                detail.retryable, !mutation,
                "only a read-only body timeout permits safe retry"
            );
        }
    }
}

//! rcoder-control HTTP 客户端
//!
//! 封装对 rcoder-control 的内部 HTTP 调用：
//! - `POST /internal/pod/ensure` → 按 identifier 确保 Pod 存在
//! - `GET /internal/session/{session_id}/resolve` → 解析 session_id → identifier

/// rcoder-control HTTP 客户端
#[derive(Clone)]
pub struct ControlPlaneClient {
    base_url: String,
    client: reqwest::Client,
}

/// internal/pod/ensure 响应
#[derive(Debug, serde::Deserialize)]
pub struct EnsurePodResponse {
    pub success: bool,
    pub data: Option<EnsurePodData>,
    pub message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsurePodData {
    pub container_name: String,
}

/// session resolve 响应
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
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { base_url, client }
    }

    /// 确保 Pod 存在（调用 rcoder-control 内部端点）
    ///
    /// 返回 container_name，用于构建 backend_cluster 名
    pub async fn ensure_pod(
        &self,
        identifier: &str,
        service_type: &str,
    ) -> anyhow::Result<EnsurePodResponse> {
        let url = format!("{}/internal/pod/ensure", self.base_url);
        let body = serde_json::json!({
            "identifier": identifier,
            "service_type": service_type,
        });
        let resp = self.client.post(&url).json(&body).send().await?;
        Self::check_status(&resp, &url)?;
        let result: EnsurePodResponse = resp.json().await?;
        Ok(result)
    }

    /// 解析 session_id → (identifier, service_type)
    pub async fn resolve_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<SessionResolveResponse> {
        let url = format!(
            "{}/internal/session/{}/resolve",
            self.base_url, session_id
        );
        let resp = self.client.get(&url).send().await?;
        Self::check_status(&resp, &url)?;
        let result: SessionResolveResponse = resp.json().await?;
        Ok(result)
    }

    fn check_status(resp: &reqwest::Response, url: &str) -> anyhow::Result<()> {
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!(
                "rcoder-control returned {} for {}",
                status,
                url
            );
        }
        Ok(())
    }
}

/// 安全构建 backend_cluster 名
///
/// 格式: `backend-{sanitized_identifier}`
/// 与 docker_manager 的 K8sBackendCRDOps::backend_crd_name 保持一致
pub fn build_backend_cluster_name(identifier: &str) -> String {
    let sanitized = identifier
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    format!("backend-{}", sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_backend_cluster_name() {
        assert_eq!(build_backend_cluster_name("user-123"), "backend-user-123");
        assert_eq!(build_backend_cluster_name("User_123"), "backend-user-123");
        assert_eq!(
            build_backend_cluster_name("user@example"),
            "backend-user-example"
        );
    }
}

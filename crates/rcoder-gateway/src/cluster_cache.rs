//! (service_type, identifier) → 控制面核验过的 container_name TTL 缓存。

use crate::control_plane_client::{ControlPlaneClient, diagnostic};
use moka::future::Cache;
use shared_types::{AppError, ServiceType, error_codes};
use std::time::Duration;
use tracing::debug;

pub struct ClusterCache {
    cache: Cache<(ServiceType, String), String>,
    control_client: ControlPlaneClient,
}

impl ClusterCache {
    pub fn new(control_client: ControlPlaneClient, ttl: Duration) -> Self {
        Self {
            cache: Cache::builder()
                .time_to_idle(ttl)
                .max_capacity(10_000)
                .build(),
            control_client,
        }
    }

    pub async fn get_or_ensure(
        &self,
        identifier: &str,
        service_type: ServiceType,
    ) -> Result<String, AppError> {
        let key = (service_type, identifier.to_owned());
        if let Some(container_name) = self.cache.get(&key).await {
            return Ok(container_name);
        }
        let response = self
            .control_client
            .ensure_pod(identifier, &service_type.to_string())
            .await?;
        let container_name = response.data.map(|data| data.container_name).filter(|name| {
            !name.is_empty() && name.len() <= 59
                && name.bytes().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
                && !name.starts_with('-') && !name.ends_with('-')
        }).ok_or_else(|| diagnostic(
            error_codes::ERR_CONTAINER_ADDRESS_NOT_READY, error_codes::ERR_CONTAINER_ADDRESS_NOT_READY,
            "gateway_target", "The control plane did not provide a valid container name for the Service target", false,
        ))?;
        debug!(service_type = %service_type, identifier, container_name, "gateway target verified");
        self.cache.insert(key, container_name.clone()).await;
        Ok(container_name)
    }

    /// 冷缓存只返回 None，由网关转发控制面只读端点；不调用 ensure。
    pub async fn get_only(&self, identifier: &str, service_type: ServiceType) -> Option<String> {
        self.cache.get(&(service_type, identifier.to_owned())).await
    }

    pub async fn insert(&self, identifier: &str, service_type: ServiceType, container_name: &str) {
        self.cache
            .insert(
                (service_type, identifier.to_owned()),
                container_name.to_owned(),
            )
            .await;
    }

    pub async fn invalidate(&self, identifier: &str, service_type: ServiceType) {
        self.cache
            .invalidate(&(service_type, identifier.to_owned()))
            .await;
    }
}
#[cfg(test)]
mod implementation_stage3_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn fixture() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture bind");
        let address = listener.local_addr().expect("fixture addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.expect("fixture accept");
                let mut input = Vec::new();
                loop {
                    let mut bytes = [0_u8; 4096];
                    let read = socket.read(&mut bytes).await.expect("fixture request");
                    if read == 0 {
                        break;
                    }
                    input.extend_from_slice(&bytes[..read]);
                    if let Some(end) = input.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let body = if String::from_utf8_lossy(&input).contains("computer-agent-runner") {
                    r#"{"success":true,"data":{"containerName":"rcoder-computer-shared"}}"#
                } else {
                    r#"{"success":true,"data":{"containerName":"rcoder-web-shared"}}"#
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(reply.as_bytes())
                    .await
                    .expect("fixture response");
            }
        });
        (format!("http://{address}"), requests, server)
    }

    #[tokio::test]
    async fn gateway_cache_uses_control_plane_container_name() {
        let (base, _, server) = fixture().await;
        let cache = ClusterCache::new(ControlPlaneClient::new(base), Duration::from_secs(60));
        let result = cache
            .get_or_ensure("shared", ServiceType::ComputerAgentRunner)
            .await
            .expect("control response");
        server.abort();
        drop(server.await);
        assert_eq!(
            result, "rcoder-computer-shared",
            "cache must preserve authority rather than invent backend name"
        );
    }

    #[tokio::test]
    async fn gateway_cache_isolates_same_identifier_across_service_types() {
        let (base, requests, server) = fixture().await;
        let cache = ClusterCache::new(ControlPlaneClient::new(base), Duration::from_secs(60));
        let computer = cache
            .get_or_ensure("shared", ServiceType::ComputerAgentRunner)
            .await
            .expect("computer ensure");
        let web = cache
            .get_or_ensure("shared", ServiceType::WebAgentRunner)
            .await
            .expect("web ensure");
        server.abort();
        drop(server.await);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "different services must not share one cached target"
        );
        assert_ne!(computer, web);
    }
    #[tokio::test]
    async fn gateway_read_only_cache_miss_never_calls_control_ensure() {
        let (base, requests, server) = fixture().await;
        let cache = ClusterCache::new(ControlPlaneClient::new(base), Duration::from_secs(60));
        assert_eq!(
            cache
                .get_only("shared", ServiceType::ComputerAgentRunner)
                .await,
            None
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        server.abort();
        drop(server.await);
    }

    #[tokio::test]
    async fn gateway_read_only_cache_hit_keeps_service_identity() {
        let (base, requests, server) = fixture().await;
        let cache = ClusterCache::new(ControlPlaneClient::new(base), Duration::from_secs(60));
        cache
            .get_or_ensure("shared", ServiceType::ComputerAgentRunner)
            .await
            .expect("ensure");
        assert_eq!(
            cache
                .get_only("shared", ServiceType::ComputerAgentRunner)
                .await
                .as_deref(),
            Some("rcoder-computer-shared")
        );
        assert_eq!(
            cache.get_only("shared", ServiceType::WebAgentRunner).await,
            None
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
        drop(server.await);
    }
}

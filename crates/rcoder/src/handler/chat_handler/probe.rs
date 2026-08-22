//! agent_runner 就绪探活（从 chat_handler.rs 拆出）。

use std::sync::Arc;
use tracing::{debug, warn};

use docker_manager::ContainerBasicInfo;

use crate::router::AppState;
use crate::*;

/// 2.5 主动探测 agent_runner gRPC 是否就绪
/// K8s 下容器刚创建时 Pod 虽已 Ready，但 agent_runner 的 gRPC server 可能仍在启动，
/// 直接转发 Chat RPC 会 transport error。这里仿照 computer_chat_handler 做状态探活，
/// 并加重试以真正等待 gRPC server 就绪（正常情况下首次即成功，无额外延迟）。
pub(crate) async fn probe_agent_runner_readiness(
    state: &Arc<AppState>,
    container_info: &ContainerBasicInfo,
    project_id: &str,
    locale: &'static str,
) {
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &container_info.container_name,
        &container_info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );

    debug!(
        "[CHAT] Probing agent_runner readiness before forward: addr={}",
        grpc_addr
    );
    const MAX_PROBE_ATTEMPTS: u32 = 6;
    for attempt in 1..=MAX_PROBE_ATTEMPTS {
        let status_req = shared_types::grpc::GetStatusRequest {
            project_id: project_id.to_string(),
            session_id: String::new(),
        };
        let mut grpc_request = grpc::new_request_with_locale(status_req, locale);
        grpc_request.set_timeout(std::time::Duration::from_secs(3));

        let probed = match state.grpc_pool.get_client(&grpc_addr).await {
            Ok(mut client) => match client.get_status(grpc_request).await {
                Ok(resp) => {
                    debug!(
                        "📊 [CHAT] Agent ready: project_id={}, status={}, attempt={}",
                        project_id,
                        resp.into_inner().status,
                        attempt
                    );
                    true
                }
                Err(e) => {
                    warn!(
                        "⚠️ [CHAT] Agent status probe failed (attempt {}/{}): {}",
                        attempt, MAX_PROBE_ATTEMPTS, e
                    );
                    // 探活失败驱逐坏 channel，避免后续重试复用同一失效连接（对齐 forward 重试 remove）
                    state.grpc_pool.remove(&grpc_addr).await;
                    false
                }
            },
            Err(e) => {
                warn!(
                    "⚠️ [CHAT] Agent status probe get_client failed (attempt {}/{}): {}",
                    attempt, MAX_PROBE_ATTEMPTS, e
                );
                // 取不到/坏 channel 也驱逐，下次重试由连接池重建
                state.grpc_pool.remove(&grpc_addr).await;
                false
            }
        };

        if probed || attempt == MAX_PROBE_ATTEMPTS {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
    // 探活失败不阻止请求，保留原有降级行为（由后续 Chat RPC 自行处理）
}

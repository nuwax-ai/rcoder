//! agent_runner 就绪探活（从 chat_handler.rs 拆出）。

use std::sync::Arc;
use tracing::debug;

use docker_manager::ContainerBasicInfo;

use crate::grpc;
use crate::router::AppState;

/// 2.5 主动探测 agent_runner gRPC 是否就绪
/// K8s 下容器刚创建时 Pod 虽已 Ready，但 agent_runner 的 gRPC server 可能仍在启动，
/// 直接转发 Chat RPC 会 transport error。这里仿照 computer_chat_handler 做状态探活，
/// 并加重试以真正等待 gRPC server 就绪（正常情况下首次即成功，无额外延迟）。
pub(super) async fn probe_agent_runner_readiness(
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
    // 探活策略：6 次固定 1s 间隔 + 全错误重试（与 computer_agent_status 的
    // 白名单+指数退避不同——探活语义是"等就绪"而非"容错"）；失败驱逐坏
    // channel 的语义在共享骨架内。
    if let Err(e) = grpc::retry::call_grpc_with_retry(
        &state.grpc_pool,
        &grpc_addr,
        grpc::retry::GrpcRetryPolicy {
            attempts: 6,
            backoff: |_| std::time::Duration::from_secs(1),
            retry_on: |_| true,
            log_tag: "CHAT",
        },
        |mut client| async move {
            let status_req = shared_types::grpc::GetStatusRequest {
                project_id: project_id.to_string(),
                session_id: String::new(),
            };
            let mut grpc_request = grpc::new_request_with_locale(status_req, locale);
            grpc_request.set_timeout(std::time::Duration::from_secs(3));
            client.get_status(grpc_request).await
        },
    )
    .await
    {
        // 探活失败不阻止请求（降级放行，由后续 Chat RPC 自行处理）
        debug!("[CHAT] readiness probe exhausted (degrade pass-through): {e}");
    }
}

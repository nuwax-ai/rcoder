//! gRPC GetStatus 重试基建（自 computer_agent_status_handler 拆出；原样搬迁）。

use std::sync::Arc;
use tracing::warn;

use super::*;

/// gRPC GetStatus 最大重试次数
pub(super) const GRPC_MAX_RETRIES: u32 = 3;

/// gRPC GetStatus 请求超时时间（秒）
pub(super) const GRPC_REQUEST_TIMEOUT_SECS: u64 = 5;

/// 调用 gRPC GetStatus（带重试机制）
///
/// # 参数
/// - `pool`: gRPC 连接池
/// - `runtime`: 容器运行时
/// - `container_name`: 容器名称
/// - `fallback_ip`: 回退 IP 地址
/// - `rcoder_prefix`: RCoder 容器前缀
/// - `computer_prefix`: Computer 容器前缀
/// - `namespace`: K8s namespace
/// - `project_id`: 项目 ID
/// - `max_retries`: 最大重试次数
/// - `locale`: 语言设置
///
/// # 返回
/// - `Ok(status)`: 从 Agent 返回的状态字符串（可能的值取决于 Agent 实现，通常为 "idle", "busy", "error", "not_found" 等）
/// - `Err(e)`: gRPC 调用失败（网络错误、超时、连接失败等）
///
/// # 重试策略
/// - 仅对可重试的错误进行重试：Unavailable, DeadlineExceeded, Unknown, Internal
///
/// gRPC GetStatus 请求参数
///
/// 封装了调用 gRPC GetStatus 所需的所有参数，
/// 避免函数参数过多。
pub(super) struct GetStatusParams<'a> {
    /// gRPC 连接池
    pub(super) pool: &'a Arc<crate::grpc::GrpcChannelPool>,
    /// 容器名称
    pub(super) container_name: &'a str,
    /// 容器 IP（Docker 环境使用）
    pub(super) container_ip: &'a str,
    /// K8s namespace
    pub(super) namespace: &'a str,
    /// 项目 ID
    pub(super) project_id: &'a str,
    /// 最大重试次数
    pub(super) max_retries: u32,
    /// 语言设置
    pub(super) locale: &'static str,
    /// K8s 集群域名
    pub(super) cluster_domain: &'a str,
}

/// - 使用指数退避：100ms, 200ms, 400ms
/// - 失败后自动从连接池移除失败的连接，并重新获取容器 IP
pub(super) async fn call_grpc_get_status_with_retry(
    params: GetStatusParams<'_>,
) -> anyhow::Result<shared_types::grpc::GetStatusResponse> {
    let mut last_error = None;

    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        params.container_name,
        params.container_ip,
        params.namespace,
        params.cluster_domain,
    );

    for attempt in 1..=params.max_retries {
        // K8s Service FQDN 是稳定的，不需要重新解析
        // 直接使用原来的 FQDN 进行重试
        if attempt > 1 {
            debug!(
                "🔄 [GRPC_GET_STATUS] Retrying with same K8s Service FQDN: {}",
                grpc_addr
            );
        }

        match params.pool.get_client(&grpc_addr).await {
            Ok(mut client) => {
                let request = shared_types::grpc::GetStatusRequest {
                    project_id: params.project_id.to_string(),
                    session_id: String::new(), // 查询项目级别状态
                };

                // 设置超时
                let mut tonic_request =
                    crate::grpc::new_request_with_locale(request, params.locale);
                tonic_request
                    .set_timeout(std::time::Duration::from_secs(GRPC_REQUEST_TIMEOUT_SECS));

                match client.get_status(tonic_request).await {
                    Ok(response) => {
                        let grpc_response = response.into_inner();
                        debug!(
                            "✅ [GRPC_GET_STATUS] Attempt {} succeeded: project_id={}, status={}, is_found={}",
                            attempt,
                            params.project_id,
                            grpc_response.status,
                            grpc_response.is_found
                        );
                        return Ok(grpc_response);
                    }
                    Err(e) => {
                        // 直接判断原始 tonic::Status，避免信息丢失
                        let should_retry = matches!(
                            e.code(),
                            tonic::Code::Unavailable
                                | tonic::Code::DeadlineExceeded
                                | tonic::Code::Unknown
                                | tonic::Code::Internal
                        );

                        if should_retry && attempt < params.max_retries {
                            warn!(
                                "⚠️ [GRPC_GET_STATUS] Attempt {} failed (retryable): project_id={}, code={:?}, error={}",
                                attempt,
                                params.project_id,
                                e.code(),
                                e
                            );
                            // 从连接池移除失败的连接
                            params.pool.remove(&grpc_addr).await;
                            last_error = Some(anyhow::anyhow!("gRPC call failed: {}", e));

                            // 指数退避: 100ms, 200ms, 400ms
                            let delay_ms = 100 * (1 << (attempt - 1));
                            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        } else {
                            error!(
                                "❌ [GRPC_GET_STATUS] Attempt {} failed (non-retryable or max retries reached): project_id={}, code={:?}, error={}",
                                attempt,
                                params.project_id,
                                e.code(),
                                e
                            );
                            return Err(anyhow::anyhow!("gRPC call failed: {}", e));
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    "⚠️ [GRPC_GET_STATUS] Attempt {} to get gRPC client failed: error={}",
                    attempt, e
                );
                // 从连接池移除可能失效的连接
                params.pool.remove(&grpc_addr).await;
                last_error = Some(e);
                if attempt < params.max_retries {
                    // 指数退避: 100ms, 200ms, 400ms
                    let delay_ms = 100 * (1 << (attempt - 1));
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Unknown error")))
}

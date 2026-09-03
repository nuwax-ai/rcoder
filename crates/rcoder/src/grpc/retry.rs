//! gRPC 调用重试骨架（agent 状态查询/探活两链路共享，策略经
//! [`GrpcRetryPolicy`] 参数化——重试次数/退避/错误白名单由调用方定值）。
//!
//! 内核：get_client → call → 失败 remove 驱逐坏 channel → 退避重试。
//! get_client 失败恒重试（连接池自身重建）；RPC 错误按 `retry_on` 判定。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Channel;

use super::GrpcChannelPool;
use shared_types::grpc::agent_service_client::AgentServiceClient;

/// 重试策略（各调用方现值保值原则：computer_agent_status=3 次指数退避+code
/// 白名单；chat 探活=6 次固定 1s+全错误重试）
pub(crate) struct GrpcRetryPolicy {
    /// 最大尝试次数（含首次）
    pub attempts: u32,
    /// 退避时长（入参 attempt 从 1 起）
    pub backoff: fn(attempt: u32) -> Duration,
    /// RPC 错误是否可重试（get_client 失败不在判定内，恒重试）
    pub retry_on: fn(tonic::Code) -> bool,
    /// 日志 tag（下游按 tag 检索）
    pub log_tag: &'static str,
}

/// 指数退避：100ms, 200ms, 400ms, ...
pub(crate) fn exponential_backoff(attempt: u32) -> Duration {
    Duration::from_millis(100 * (1 << (attempt - 1)))
}

/// computer_agent_status 的可重试 code 白名单
pub(crate) fn retry_on_transport_errors(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Unknown
            | tonic::Code::Internal
    )
}

/// 泛型重试骨架：每轮重新 get_client，把 client 交调用方闭包执行 RPC。
/// 成功返回 `response.into_inner()`；重试耗尽返回最后一次错误。
pub(crate) async fn call_grpc_with_retry<R, F, Fut>(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    policy: GrpcRetryPolicy,
    mut call: F,
) -> anyhow::Result<R>
where
    F: FnMut(AgentServiceClient<Channel>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<R>, tonic::Status>>,
{
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=policy.attempts {
        if attempt > 1 {
            tracing::debug!(
                "🔄 [{}] retry attempt {}/{}: addr={}",
                policy.log_tag,
                attempt,
                policy.attempts,
                grpc_addr
            );
        }
        match pool.get_client(grpc_addr).await {
            Ok(client) => match call(client).await {
                Ok(response) => return Ok(response.into_inner()),
                Err(e) => {
                    let should_retry = (policy.retry_on)(e.code());
                    if should_retry && attempt < policy.attempts {
                        tracing::warn!(
                            "⚠️ [{}] RPC failed (retryable, attempt {}/{}): code={:?}, error={}",
                            policy.log_tag,
                            attempt,
                            policy.attempts,
                            e.code(),
                            e
                        );
                        pool.remove(grpc_addr).await;
                        last_error = Some(anyhow::anyhow!("gRPC call failed: {}", e));
                        tokio::time::sleep((policy.backoff)(attempt)).await;
                        continue;
                    }
                    tracing::error!(
                        "❌ [{}] RPC failed (non-retryable or exhausted, attempt {}/{}): code={:?}, error={}",
                        policy.log_tag,
                        attempt,
                        policy.attempts,
                        e.code(),
                        e
                    );
                    return Err(anyhow::anyhow!("gRPC call failed: {}", e));
                }
            },
            Err(e) => {
                tracing::warn!(
                    "⚠️ [{}] get_client failed (attempt {}/{}): error={}",
                    policy.log_tag,
                    attempt,
                    policy.attempts,
                    e
                );
                pool.remove(grpc_addr).await;
                last_error = Some(anyhow::anyhow!("{}", e));
                if attempt < policy.attempts {
                    tokio::time::sleep((policy.backoff)(attempt)).await;
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "gRPC call to {grpc_addr} failed after {} attempts",
            policy.attempts
        )
    }))
}

//! 统一的 chat gRPC 转发器
//!
//! Web（WebAgentRunner）与 Computer（ComputerAgentRunner）两条 chat 链路
//! 此前各自实现了相同的 `max_retries=2` 重试循环（成功透传 / 业务错误透传 /
//! 可重试错误清池重试 / 不可重试错误终止）。本模块将其收敛为单一实现，
//! 差异点通过 [`ForwardChatOpts`] 参数化：
//!
//! - `retry_delay`：Computer 链路重试前等待 3s（给容器内 gRPC 服务启动时间）；
//!   Web 链路不等待。
//! - `re_resolve`：Web 链路在 Docker 模式下重试前按 name 实时重新解析容器 IP
//!   （容器重启后 IP 漂移）；Computer 链路沿用原地址。
//!
//! 路由与响应格式保持完全不变，仅抽内部转发逻辑。

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::HttpResult;
use crate::grpc::{
    GrpcChannelPool, GrpcChatParams, grpc_chat_with_pool, grpc_response_to_chat_response,
};
use shared_types::ChatResponse;

/// chat gRPC 转发最大尝试次数
const MAX_RETRIES: u32 = 2;

/// 重试前重新解析容器地址的上下文（Docker 模式 IP 漂移场景）
pub struct ReResolveCtx<'a> {
    /// 容器运行时（按 name 实时解析）
    pub runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    /// 容器标识符（project_id / logical id）
    pub project_id: &'a str,
    /// 服务类型
    pub service_type: shared_types::ServiceType,
    /// K8s namespace
    pub namespace: &'a str,
    /// K8s 集群域名
    pub cluster_domain: &'a str,
}

/// 转发行为差异选项
pub struct ForwardChatOpts<'a> {
    /// 日志标签（区分链路，如 "FORWARD" / "COMPUTER_FORWARD"）
    pub log_tag: &'static str,
    /// 重试前等待时长（Computer 场景给 gRPC 服务启动时间；Web 传 None 不等待）
    pub retry_delay: Option<Duration>,
    /// 重试前是否重新解析容器地址（Web Docker 场景）
    pub re_resolve: Option<ReResolveCtx<'a>>,
}

/// 统一的 chat gRPC 转发（带重试）
///
/// 语义与原双实现保持一致：
/// - gRPC 业务错误（`success=false`）不重试，错误码完整透传；
/// - gRPC 通信错误按 [`crate::grpc::GrpcError::should_retry`] 决定是否重试；
/// - 全部重试失败后返回本地化的 `ERR_GRPC_ERROR`。
///
/// `make_params` 闭包在每次尝试前构造请求参数（各尝试间参数相同）。
pub async fn forward_chat(
    grpc_pool: &Arc<GrpcChannelPool>,
    mut grpc_addr: String,
    make_params: impl Fn() -> GrpcChatParams,
    locale: &'static str,
    opts: ForwardChatOpts<'_>,
) -> HttpResult<ChatResponse> {
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let params = make_params();

        match grpc_chat_with_pool(grpc_pool, &grpc_addr, params).await {
            Ok(grpc_response) => {
                if grpc_response.success {
                    // 转换为内部 ChatResponse
                    let chat_response = grpc_response_to_chat_response(grpc_response);
                    info!(
                        "✅ [{}] gRPC response success: project_id={}, session_id={}",
                        opts.log_tag, chat_response.project_id, chat_response.session_id
                    );
                    return HttpResult::success(chat_response);
                } else {
                    let error_msg = grpc_response
                        .error
                        .unwrap_or_else(|| "Unknown error".to_string());
                    // 🎯 从 gRPC 响应中提取错误码（完整透传）
                    let error_code = grpc_response
                        .error_code
                        .unwrap_or_else(|| shared_types::error_codes::ERR_AGENT_ERROR.to_string());
                    error!(
                        "❌ [{}] gRPC response error: code={}, message={}",
                        opts.log_tag, error_code, error_msg
                    );
                    return HttpResult::error(&error_code, &error_msg);
                }
            }
            Err(grpc_err) => {
                warn!(
                    "⚠️ [{}] gRPC call failed (attempt {}/{}): {}",
                    opts.log_tag, attempt, MAX_RETRIES, grpc_err
                );

                // ✅ 使用 GrpcError 的 should_retry 方法，无需 downcast_ref
                let should_retry = grpc_err.should_retry();

                if should_retry && attempt < MAX_RETRIES {
                    // 可重试错误：（可选等待）+ 清理连接池后重试
                    if let Some(delay) = opts.retry_delay {
                        info!(
                            "🔄 [{}] Detected retryable error, waiting {:?} before retry...",
                            opts.log_tag, delay
                        );
                        tokio::time::sleep(delay).await;
                    } else {
                        info!(
                            "🔄 [{}] Detected retryable error, retrying...",
                            opts.log_tag
                        );
                    }
                    grpc_pool.remove(&grpc_addr).await;

                    // K8s Service FQDN 稳定无需重解析；Docker 模式容器重启后 IP 可能变化，
                    // 按 name 实时重新解析（find_container → find_container_realtime，
                    // 不要用 get_container_info_by_identifier：它走缓存的旧 container_id 会 404）。
                    if let Some(rr) = &opts.re_resolve
                        && !shared_types::is_kubernetes_runtime()
                    {
                        match rr
                            .runtime
                            .find_container(rr.project_id, &rr.service_type)
                            .await
                        {
                            Ok(Some(info)) if !info.container_ip.is_empty() => {
                                let new_addr = shared_types::build_grpc_addr(
                                    &info.container_name,
                                    &info.container_ip,
                                    rr.namespace,
                                    rr.cluster_domain,
                                );
                                if new_addr != grpc_addr {
                                    info!(
                                        "🔄 [{}] Container IP changed on retry: {} -> {}",
                                        opts.log_tag, grpc_addr, new_addr
                                    );
                                    grpc_addr = new_addr;
                                }
                            }
                            other => {
                                warn!(
                                    "🔄 [{}] Re-resolve on retry returned {:?}, retrying with original addr: {}",
                                    opts.log_tag, other, grpc_addr
                                );
                            }
                        }
                    } else {
                        debug!(
                            "🔄 [{}] Retrying with same gRPC address: {}",
                            opts.log_tag, grpc_addr
                        );
                    }

                    last_error = Some(anyhow::Error::from(grpc_err));
                    continue;
                } else if !should_retry {
                    // 不可重试错误：直接返回
                    error!(
                        "[{}] Detected non-retryable error, stopped retry: {}",
                        opts.log_tag, grpc_err
                    );
                    last_error = Some(anyhow::Error::from(grpc_err));
                    break;
                }

                // 最后一次尝试失败
                last_error = Some(anyhow::Error::from(grpc_err));
            }
        }
    }

    // 所有重试都失败
    if let Some(e) = last_error {
        error!(
            "❌ [{}] gRPC request failed after all retries: {}",
            opts.log_tag, e
        );
    }

    // gRPC 通信失败，直接返回错误
    // 注：业务错误码（如 Agent busy）由 agent_runner 通过 grpc_response.error_code 返回，
    // 这里只处理真正的 gRPC 通信层错误
    HttpResult::error_with_locale(shared_types::error_codes::ERR_GRPC_ERROR, locale)
}

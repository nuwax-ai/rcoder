//! 统一的 chat gRPC 转发器
//!
//! Web（WebAgentRunner）与 Computer（ComputerAgentRunner）两条 chat 链路
//! 此前各自实现了相同的 `max_retries=2` 重试循环（成功透传 / 业务错误透传 /
//! 可重试错误清池重试 / 不可重试错误终止）。本模块将其收敛为单一实现，
//! 差异点通过 [`ForwardChatOpts`] 参数化：
//!
//! - `retry_delay`：Computer 链路重试前等 gRPC 端口就绪（探测上限 30s，
//!   覆盖容器冷启动窗口）；Web 链路不等待。
//! - `re_resolve`：Web 链路在 Docker 模式下重试前按 name 实时重新解析容器 IP
//!   （容器重启后 IP 漂移）；Computer 链路沿用原地址。
//!
//! 路由与响应格式保持完全不变，仅抽内部转发逻辑。

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, instrument, warn};

use crate::HttpResult;
use crate::grpc::{
    GrpcChannelPool, GrpcChatParams, grpc_chat_with_pool, grpc_response_to_chat_response,
};
use shared_types::ChatResponse;

/// chat gRPC 转发最大尝试次数
const MAX_RETRIES: u32 = 2;

/// 智能等待 pod ready 的超时上限(容器冷启动 / OOM 重启恢复窗口)
const AGENT_READY_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// 重试前 gRPC 端口就绪探测上限：覆盖 agent 容器 start-up.sh 冷启动窗口
/// （本地实测 ~3s / 生产同源版 ~15s，含并发资源争抢余量）
const PORT_READY_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// pod ready 后给 gRPC server 的缓冲(readiness probe 通过 ≠ gRPC 立即可连)
const AGENT_READY_RETRY_BUFFER: Duration = Duration::from_secs(1);

/// chat 处理流程的阶段退出点（纯控制流，不含业务逻辑）
///
/// chat / computer-chat 两条链路在拆分为多阶段函数后，阶段失败分为两类：
/// - [`ChatFlowExit::Response`]：业务校验/容器错误，按原语义以 `Ok(HttpResult::error...)`
///   形式返回客户端（HTTP 200 + 错误体）；
/// - [`ChatFlowExit::Fatal`]：基础设施错误，按原语义以 `Err(AppError)` 向上传播。
pub enum ChatFlowExit {
    /// 以业务错误响应提前结束（保持原 `Ok(HttpResult::...)` 语义）
    Response(HttpResult<ChatResponse>),
    /// 以 AppError 提前结束（保持原 `Err(AppError)` 语义）
    Fatal(crate::AppError),
}

impl From<crate::AppError> for ChatFlowExit {
    fn from(e: crate::AppError) -> Self {
        ChatFlowExit::Fatal(e)
    }
}

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

/// 诊断上下文:gRPC 连接失败时定位真实根因(OOM/CrashLoop/缺失)+ 智能等待 pod ready。
pub struct DiagnosticCtx<'a> {
    /// 容器运行时(查 pod 状态)
    pub runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    /// 容器标识(ComputerAgentRunner=user_id; WebAgentRunner=project_id)
    pub identifier: String,
    pub service_type: shared_types::ServiceType,
}

/// 转发行为差异选项
pub struct ForwardChatOpts<'a> {
    /// 日志标签（区分链路，如 "FORWARD" / "COMPUTER_FORWARD"）
    pub log_tag: &'static str,
    /// 重试前等待语义开关：Some(_) = 等 gRPC 端口就绪（探测上限 30s，Computer 场景；
    /// 具体时长值不再使用）；None = 立即重试（Web 场景）
    pub retry_delay: Option<Duration>,
    /// 重试前是否重新解析容器地址（Web Docker 场景）
    pub re_resolve: Option<ReResolveCtx<'a>>,
    /// 连接失败时定位真实根因(OOM/CrashLoop/缺失)+ 智能等待 ready。None=不诊断(原行为)。
    pub diagnostic: Option<DiagnosticCtx<'a>>,
}

/// 统一的 chat gRPC 转发（带重试）
///
/// 语义与原双实现保持一致：
/// - gRPC 业务错误（`success=false`）不重试，错误码完整透传；
/// - gRPC 通信错误按 [`crate::grpc::GrpcError::should_retry`] 决定是否重试；
/// - 全部重试失败后返回本地化的 `ERR_GRPC_ERROR`。
///
/// `make_params` 闭包在每次尝试前构造请求参数（各尝试间参数相同）。
///
/// span 覆盖整个 turn 等待（含重试/智能等待）——chat POST 的墙钟大头；
/// 耗时指标由 SpanMetricsLayer 从 span 自动记录（method="chat"）；
/// 出口仅记录业务结果计数。
#[instrument(skip_all, fields(tag = %opts.log_tag))]
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
                    rcoder_telemetry::prometheus::record_grpc_request("chat", "ok");
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
                    rcoder_telemetry::prometheus::record_grpc_request("chat", "error");
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
                    // 🧠 智能等待:若诊断出容器启动中/OOM 重启,等 pod ready 再重试
                    // (替代固定 sleep —— 旧策略对 30s+ 启动窗口无能为力)
                    if let Some(dc) = &opts.diagnostic {
                        let d = crate::handler::utils::diagnose(
                            dc.runtime,
                            &dc.identifier,
                            dc.service_type.clone(),
                        )
                        .await;
                        if d.is_starting_up() || d.is_oom() {
                            info!(
                                "🔄 [{}] 容器启动中/OOM 重启中,智能等待 pod ready (最多 60s)...",
                                opts.log_tag
                            );
                            let ready = crate::handler::utils::wait_agent_ready(
                                dc.runtime,
                                &dc.identifier,
                                dc.service_type.clone(),
                                AGENT_READY_WAIT_TIMEOUT,
                            )
                            .await;
                            if ready {
                                grpc_pool.remove(&grpc_addr).await;
                                last_error = Some(anyhow::Error::from(grpc_err));
                                // readiness 通过 ≠ gRPC server 立即可连,给一小段缓冲再重试
                                tokio::time::sleep(AGENT_READY_RETRY_BUFFER).await;
                                info!("✅ [{}] pod 已 ready,重试", opts.log_tag);
                                continue;
                            }
                            // 未 ready(超时/崩溃):落到下面固定 sleep 做兜底重试
                        }
                    }

                    // 可重试错误：（可选等待）+ 清理连接池后重试。
                    // Computer 链路（retry_delay>0）用端口就绪探测替代固定 sleep：
                    // 探测到 gRPC 端口监听立即重试（冷启动窗口内精确等待），
                    // 超时（30s 上限）按原重试语义兜底继续——原固定 3s sleep 在
                    // 生产 ~15s 冷启动窗口下要盲等多轮且每轮 dial 白耗超时
                    if let Some(_delay) = opts.retry_delay {
                        let ready = crate::grpc::port_ready::wait_grpc_port_ready(
                            &grpc_addr,
                            PORT_READY_WAIT_TIMEOUT,
                        )
                        .await;
                        info!(
                            "🔄 [{}] Detected retryable error, gRPC 端口探测{}, retrying...",
                            opts.log_tag,
                            if ready {
                                "就绪"
                            } else {
                                "超时(兜底继续)"
                            }
                        );
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
    if let Some(e) = &last_error {
        error!(
            "❌ [{}] gRPC request failed after all retries: {}",
            opts.log_tag, e
        );
    }

    // gRPC 通信失败，直接返回错误
    // 注：业务错误码（如 Agent busy）由 agent_runner 通过 grpc_response.error_code 返回，
    // 这里只处理真正的 gRPC 通信层错误

    // 有诊断上下文:根据真实根因生成友好错误
    // (有根因 → ERR_AGENT_CONTAINER_UNAVAILABLE + 根因; 无根因 → 保留 transport 原文)
    if let Some(dc) = &opts.diagnostic {
        let raw = last_error
            .as_ref()
            .map(|e| format!("{e}"))
            .unwrap_or_default();
        let (code, msg) = crate::handler::utils::build_connection_error(
            dc.runtime,
            &dc.identifier,
            dc.service_type.clone(),
            locale,
            &raw,
        )
        .await;
        rcoder_telemetry::prometheus::record_grpc_request("chat", "error");
        return HttpResult::error(code.as_str(), msg.as_str());
    }

    // 无诊断上下文:保留原行为(通用 ERR_GRPC_ERROR)
    rcoder_telemetry::prometheus::record_grpc_request("chat", "error");
    HttpResult::error_with_locale(shared_types::error_codes::ERR_GRPC_ERROR, locale)
}

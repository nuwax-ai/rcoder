//! SSE 响应流构建
//!
//! 从 `agent_session_notification` 迁出：封装 gRPC SSE 流的构建逻辑，
//! 供 `agent_session_notification` 与 `computer_agent_progress_notification` 共用。

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tracing::info;

use crate::AppError;

/// SSE 流构建参数
///
/// 封装了构建 gRPC SSE 流所需的所有参数，
/// 避免函数参数过多，同时支持不同场景的扩展。
pub(crate) struct SseStreamParams {
    /// 容器名称
    pub(crate) container_name: String,
    /// 容器 IP（Docker 环境使用）
    pub(crate) container_ip: String,
    /// 会话 ID
    pub(crate) session_id: String,
    /// 项目 ID
    pub(crate) project_id: String,
    /// gRPC 连接池
    pub(crate) grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    /// 语言设置
    pub(crate) locale: &'static str,
    /// 服务类型（用于日志区分不同类型的 Agent）
    pub(crate) service_type: shared_types::ServiceType,
    /// 活动更新器
    pub(crate) activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
    /// K8s namespace
    pub(crate) namespace: String,
    /// K8s 集群域名
    pub(crate) cluster_domain: String,
    /// Session 共享流注册表（每 session 一条 agent_runner 流，多 SSE 客户端 fan-out 共享）
    pub(crate) registry: Arc<crate::grpc::SessionStreamRegistry>,
    /// 诊断上下文：SSE 流断开重试耗尽时，据此做 OOM/CrashLoop 等精准诊断，替代通用文案。
    /// None（无 runtime / 拿不到 identifier）→ 通用 "Compute environment temporarily unavailable"。
    pub(crate) diag_ctx: Option<Arc<crate::handler::utils::DiagCtx>>,
    /// 客户端消费游标（从 Last-Event-ID header 或 ?last_seq= query 读取），
    /// 用于增量补齐；缺省 0 = 补齐该 session 全量历史（首次连接合理，重连应由前端带 last_seq）。
    pub(crate) last_seq: u64,
}

/// 这个函数被 agent_session_notification 和 computer_agent_progress_notification 共同使用
/// 通过 container_name 创建 gRPC SSE 流
pub(crate) async fn build_sse_stream_from_container_name(
    params: SseStreamParams,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &params.container_name,
        &params.container_ip,
        &params.namespace,
        &params.cluster_domain,
    );
    info!(
        "[gRPC_SSE] Establishing {} gRPC SSE proxy connection: {}, project_id={}",
        params.service_type, grpc_addr, params.project_id
    );

    // 创建 gRPC SSE 流
    let stream = crate::grpc::create_grpc_sse_stream(
        params.registry.clone(),
        grpc_addr,
        params.session_id.clone(),
        params.grpc_pool.clone(),
        params.locale,
        params.activity_updater,
        params.diag_ctx,
        params.last_seq,
    )
    .await;

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

//! 容器活跃状态 gRPC 查询（agent 健康检查链共享）。
//!
//! cleanup scanner 与 container_status_checker 周期任务原先各持一份
//! ~20 行逐字实现（GetContainerStatusRequest → is_active || active_tasks > 0），
//! 收敛为单一事实源。timeout 仅覆盖 RPC 本身；get_client 的覆盖由调用方
//! 按需再包外层超时（scanner 的"状态未知"语义依赖外层包裹）。

use std::sync::Arc;
use std::time::Duration;

use tokio::time;

use super::GrpcChannelPool;
use shared_types::grpc::{GetContainerStatusRequest, GetContainerStatusResponse};

/// 查询容器内 agent 活跃状态，返回完整响应（调用方各自打上下文日志）。
pub(crate) async fn query_container_status(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    user_id: &str,
    project_id: &str,
    query_timeout: Duration,
) -> anyhow::Result<GetContainerStatusResponse> {
    let mut client = pool.get_client(grpc_addr).await?;
    let request = tonic::Request::new(GetContainerStatusRequest {
        user_id: user_id.to_string(),
        project_id: project_id.to_string(),
    });
    let response = time::timeout(query_timeout, client.get_container_status(request)).await??;
    Ok(response.into_inner())
}

/// 活跃判定（与 gRPC 响应字段语义绑定的单一事实源）：
/// 有活跃任务或 agent 自报 is_active 即视为活跃（不应清理）。
pub(crate) fn is_agent_active(status: &GetContainerStatusResponse) -> bool {
    status.is_active || status.active_tasks > 0
}

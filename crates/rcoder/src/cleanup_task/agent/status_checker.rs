//! Agent 状态检查器
//!
//! 通过 gRPC 查询容器内 agent 的真实状态

use anyhow::Result;
use shared_types::grpc::GetContainerStatusRequest;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::debug;

/// Agent 状态检查器
#[derive(Clone)]
pub struct AgentStatusChecker {
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
}

impl AgentStatusChecker {
    pub fn new(grpc_pool: Arc<crate::grpc::GrpcChannelPool>) -> Self {
        Self { grpc_pool }
    }

    /// 查询容器内 agent 是否正在执行任务
    ///
    /// 返回 true 表示活跃（不应清理），false 表示空闲（可以清理）
    pub async fn is_container_active(
        &self,
        grpc_addr: &str,
        user_id: &str,
        project_id: &str,
    ) -> Result<bool> {
        let timeout_duration = Duration::from_secs(3);

        match timeout(
            timeout_duration,
            self.query_container_status(grpc_addr, user_id, project_id),
        )
        .await
        {
            Ok(Ok(is_active)) => Ok(is_active),
            Ok(Err(e)) => {
                // gRPC 失败 = 状态"未知",不是"空闲"。若当 idle 清理,会误杀刚 OOM 重启、
                // 进程尚未就绪的 agent(gRPC 窗口期连不上)。改返 Err,让 scanner 本轮跳过。
                debug!(" [status_checker] gRPC Query failed (state unknown): {}", e);
                Err(e)
            }
            Err(_) => {
                debug!(" [status_checker] gRPC timeout (state unknown)");
                Err(anyhow::anyhow!("gRPC query timeout"))
            }
        }
    }

    async fn query_container_status(
        &self,
        grpc_addr: &str,
        user_id: &str,
        project_id: &str,
    ) -> Result<bool> {
        let mut client = self.grpc_pool.get_client(grpc_addr).await?;

        let request = tonic::Request::new(GetContainerStatusRequest {
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
        });

        let response = client.get_container_status(request).await?;
        let status = response.into_inner();

        debug!(
            " [status_checker] Container status: is_active={}, active_tasks={}",
            status.is_active, status.active_tasks
        );

        Ok(status.is_active || status.active_tasks > 0)
    }
}

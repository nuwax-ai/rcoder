//! Agent HTTP Service Trait
//!
//! 定义 Agent HTTP 服务的抽象接口，供 RCoder 和 Agent Runner 两端实现
//!
//! ## Architecture
//!
//! - **RCoder 实现** (`GrpcAgentHttpService`): 通过 gRPC 代理转发到容器内 Agent Runner
//! - **Agent Runner 实现** (`LocalAgentHttpService`): 直接调用本地 AGENT_REGISTRY/SESSION_CACHE

use async_trait::async_trait;

use crate::{
    AgentStatusResponse, ChatResponse, HttpResult,
    rcoder_agent_types::{RcoderAgentCancelRequest, RcoderAgentCancelResponse, RcoderAgentStopRequest, RcoderAgentStopResponse},
};

/// Agent HTTP 服务抽象 trait
///
/// 实现方可以是：
/// - RCoder: gRPC 代理到容器内 Agent Runner
/// - Agent Runner: 直接调用本地服务
#[async_trait]
pub trait AgentHttpService: Send + Sync + 'static {
    /// Chat 对话请求
    async fn chat(&self, request: crate::rcoder_agent_types::RcoderChatRequest) -> HttpResult<ChatResponse>;

    /// 查询 Agent 状态
    async fn get_status(&self, project_id: &str) -> HttpResult<AgentStatusResponse>;

    /// 停止 Agent
    async fn stop(&self, request: RcoderAgentStopRequest) -> HttpResult<RcoderAgentStopResponse>;

    /// 取消正在执行的任务
    async fn cancel(&self, request: RcoderAgentCancelRequest) -> HttpResult<RcoderAgentCancelResponse>;
}
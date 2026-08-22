//! chat 转发上下文与路由目标类型（从 chat_handler.rs 拆出）。

use std::sync::Arc;

use crate::*;

/// 转发请求的上下文参数
///
/// 封装了转发请求到容器服务所需的所有参数，
/// 避免函数参数过多，同时支持不同 ServiceType 的业务场景。
///
/// 注意：runtime、rcoder_prefix、computer_prefix 字段
/// 当前在 forward_request_to_container_service 中未使用，
/// 但保留用于未来扩展不同 ServiceType 的业务逻辑。
#[allow(dead_code)]
pub(crate) struct ForwardContext<'a> {
    /// gRPC 连接池
    pub(crate) grpc_pool: &'a Arc<grpc::GrpcChannelPool>,
    /// K8s namespace
    pub(crate) namespace: &'a str,
    /// K8s 集群域名
    pub(crate) cluster_domain: &'a str,
    /// 容器运行时（用于不同 ServiceType 的容器管理）
    pub(crate) runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    /// RCoder 容器前缀（用于 WebAgentRunner 场景）
    pub(crate) rcoder_prefix: &'a str,
    /// Computer 容器前缀（用于 ComputerAgentRunner 场景）
    pub(crate) computer_prefix: &'a str,
    /// 语言设置
    pub(crate) locale: &'static str,
}

/// 请求校验与路由解析的结果
pub(crate) struct ChatRouteTarget {
    /// 项目 ID（缺省时自动生成）
    pub(crate) project_id: String,
    /// 工作目录标识符（agent_work_dir 或 project_id）
    pub(crate) work_dir_id: String,
    /// 容器内工作空间路径
    pub(crate) container_work_path: String,
}

//! gRPC GetStatus 重试基建（自 computer_agent_status_handler 拆出；原样搬迁）。

use std::sync::Arc;
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
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）；
    // FQDN 稳定不重解析，重试轮间复用同一地址
    let grpc_addr = shared_types::build_grpc_addr(
        params.container_name,
        params.container_ip,
        params.namespace,
        params.cluster_domain,
    );

    crate::grpc::retry::call_grpc_with_retry(
        params.pool,
        &grpc_addr,
        crate::grpc::retry::GrpcRetryPolicy {
            attempts: params.max_retries,
            backoff: crate::grpc::retry::exponential_backoff,
            retry_on: crate::grpc::retry::retry_on_transport_errors,
            log_tag: "GRPC_GET_STATUS",
        },
        |mut client| async move {
            let request = shared_types::grpc::GetStatusRequest {
                project_id: params.project_id.to_string(),
                session_id: String::new(), // 查询项目级别状态
            };
            let mut tonic_request = crate::grpc::new_request_with_locale(request, params.locale);
            tonic_request.set_timeout(std::time::Duration::from_secs(GRPC_REQUEST_TIMEOUT_SECS));
            client.get_status(tonic_request).await
        },
    )
    .await
}

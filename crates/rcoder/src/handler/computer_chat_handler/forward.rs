use super::*;

///
/// 封装了转发 Computer Chat 请求到容器服务所需的所有参数，
/// 避免函数参数过多，同时支持不同场景的扩展。
pub(super) struct ComputerForwardParams<'a> {
    /// Computer Chat 请求
    pub(super) request: &'a ComputerChatRequest,
    /// 项目 ID
    pub(super) project_id: &'a str,
    /// 工作目录 ID
    pub(super) work_dir_id: &'a str,
    /// 容器信息
    pub(super) container_info: &'a ContainerBasicInfo,
    /// gRPC 连接池
    pub(super) grpc_pool: &'a Arc<crate::grpc::GrpcChannelPool>,
    /// 语言设置
    pub(super) locale: &'static str,
    /// 是否是 DevComputer 接口请求
    pub(super) is_devcomputer: bool,
    /// K8s namespace
    pub(super) namespace: &'a str,
    /// K8s 集群域名
    pub(super) cluster_domain: &'a str,
    /// 容器运行时(连接失败时诊断 pod 真实根因)
    pub(super) runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
}

/// 与 RCoder 的 forward_request_to_container_service 类似，
/// 但专门用于 ComputerAgentRunner 模式。
pub(super) async fn forward_computer_request_to_container(
    params: ComputerForwardParams<'_>,
) -> HttpResult<ChatResponse> {
    info!(
        "📤 [COMPUTER_FORWARD] Forwarding request to container (gRPC): user_id={}, project_id={}, session_id={:?}, container_id={}, is_devcomputer={}",
        params.request.user_id,
        params.project_id,
        params.request.session_id,
        params.container_info.container_id,
        params.is_devcomputer
    );

    // 直接使用 gRPC 的健康检查机制，不额外检查容器状态
    // gRPC 连接失败会自动返回错误，由上层处理

    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &params.container_info.container_name,
        &params.container_info.container_ip,
        params.namespace,
        params.cluster_domain,
    );

    debug!(
        "📡 [COMPUTER_FORWARD] gRPC address: {}, prompt_len={}, attachments={}",
        grpc_addr,
        params.request.prompt.len(),
        params.request.attachments.len()
    );

    // Computer Agent Runner 的工作目录路径
    // 在容器内：/app/computer-project-workspace/{user_id}/{work_dir_id}
    let project_workspace = match project_dir(&params.request.user_id, params.work_dir_id) {
        Ok(path) => format!("{}/", path),
        Err(e) => {
            return HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                params.locale,
                &e.to_string(),
            );
        }
    };

    debug!(
        "[COMPUTER_FORWARD] projectworkdirectory: {}",
        project_workspace
    );

    // gRPC 调用（带重试机制；统一转发器见 chat_forward.rs）
    let request = params.request;
    let project_id = params.project_id;
    let grpc_pool = params.grpc_pool;
    crate::handler::chat_forward::forward_chat(
        grpc_pool,
        grpc_addr,
        || crate::grpc::GrpcChatParams {
            project_id: project_id.to_string(),
            session_id: request.session_id.clone(),
            prompt: request.prompt.clone(),
            attachments: request.attachments.clone(),
            data_source_attachments: request.data_source_attachments.clone(),
            model_config: request.model_provider.clone(),
            request_id: request.request_id.clone(),
            request_timeout: Some(std::time::Duration::from_secs(
                shared_types::GRPC_CHAT_TIMEOUT_SECS,
            )),
            system_prompt: request.system_prompt.clone(),
            user_prompt: request.user_prompt.clone(),
            agent_config: request.agent_config.clone(),
            service_type: Some(shared_types::ServiceType::ComputerAgentRunner),
            user_id: Some(request.user_id.clone()),
            is_devcomputer: params.is_devcomputer,
            agent_work_dir: request.agent_work_dir.clone(),
        },
        params.locale,
        crate::handler::chat_forward::ForwardChatOpts {
            log_tag: "COMPUTER_FORWARD",
            // 重试前等待 3s，给容器内 gRPC 服务启动时间（保持原有行为）
            retry_delay: Some(std::time::Duration::from_secs(3)),
            // K8s Service FQDN 稳定，Computer 链路不需要重新解析
            re_resolve: None,
            // 连接失败时诊断 pod 根因(OOM/CrashLoop/缺失)+ 智能等待 ready
            diagnostic: Some(crate::handler::chat_forward::DiagnosticCtx {
                runtime: params.runtime,
                identifier: params.request.user_id.clone(),
                service_type: shared_types::ServiceType::ComputerAgentRunner,
            }),
        },
    )
    .await
}

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

    // gRPC 调用（带重试机制）
    let max_retries = 2;
    let mut last_error = None;

    for attempt in 1..=max_retries {
        let grpc_params = crate::grpc::GrpcChatParams {
            project_id: params.project_id.to_string(),
            session_id: params.request.session_id.clone(),
            prompt: params.request.prompt.clone(),
            attachments: params.request.attachments.clone(),
            data_source_attachments: params.request.data_source_attachments.clone(),
            model_config: params.request.model_provider.clone(),
            request_id: params.request.request_id.clone(),
            request_timeout: Some(std::time::Duration::from_secs(
                shared_types::GRPC_CHAT_TIMEOUT_SECS,
            )),
            system_prompt: params.request.system_prompt.clone(),
            user_prompt: params.request.user_prompt.clone(),
            agent_config: params.request.agent_config.clone(),
            service_type: Some(shared_types::ServiceType::ComputerAgentRunner),
            user_id: Some(params.request.user_id.clone()),
            is_devcomputer: params.is_devcomputer,
            agent_work_dir: params.request.agent_work_dir.clone(),
        };

        match crate::grpc::grpc_chat_with_pool(params.grpc_pool, &grpc_addr, grpc_params).await {
            Ok(grpc_response) => {
                if grpc_response.success {
                    let chat_response = crate::grpc::grpc_response_to_chat_response(grpc_response);
                    info!(
                        "✅ [COMPUTER_FORWARD] gRPC response success: project_id={}, session_id={}",
                        chat_response.project_id, chat_response.session_id
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
                        "❌ [COMPUTER_FORWARD] gRPC response error: code={}, message={}",
                        error_code, error_msg
                    );
                    return HttpResult::error(&error_code, &error_msg);
                }
            }
            Err(grpc_err) => {
                warn!(
                    "⚠️ [COMPUTER_FORWARD] gRPC call failed (attempt {}/{}): {}",
                    attempt, max_retries, grpc_err
                );

                // 使用 GrpcError 的 should_retry 方法，无需 downcast_ref
                let should_retry = grpc_err.should_retry();

                if should_retry && attempt < max_retries {
                    // 等待一段时间再重试，给 gRPC 服务启动时间
                    let retry_delay = std::time::Duration::from_secs(3);
                    info!(
                        "🔄 [COMPUTER_FORWARD] Detected retryable error, waiting {:?} before retry...",
                        retry_delay
                    );
                    tokio::time::sleep(retry_delay).await;

                    params.grpc_pool.remove(&grpc_addr).await;

                    // K8s Service FQDN 是稳定的，不需要重新解析
                    // 直接使用原来的 FQDN 进行重试
                    debug!(
                        "🔄 [COMPUTER_FORWARD] Retrying with same K8s Service FQDN: {}",
                        grpc_addr
                    );

                    last_error = Some(anyhow::Error::from(grpc_err));
                    continue;
                } else if !should_retry {
                    error!(
                        "[COMPUTER_FORWARD] Non-retryable error, stopped retry: {}",
                        grpc_err
                    );
                    last_error = Some(anyhow::Error::from(grpc_err));
                    break;
                }

                last_error = Some(anyhow::Error::from(grpc_err));
            }
        }
    }

    // 所有重试都失败
    if let Some(e) = last_error {
        error!(
            "❌ [COMPUTER_FORWARD] gRPC final call failed: {}, user_id={}, project_id={}",
            e, params.request.user_id, params.project_id
        );

        // gRPC 通信失败，直接返回错误
        // 注：业务错误码（如 Agent busy）现在由 agent_runner 通过 grpc_response.error_code 返回
        HttpResult::error_with_locale(shared_types::error_codes::ERR_GRPC_ERROR, params.locale)
    } else {
        HttpResult::error_with_locale(shared_types::error_codes::ERR_UNKNOWN, params.locale)
    }
}

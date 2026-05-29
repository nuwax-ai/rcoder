//! Chat RPC 实现

use std::sync::Arc;

use shared_types::grpc::{
    ChatRequest as GrpcChatRequest, ChatResponse as GrpcChatResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::router::AppState;
use crate::service::{ChatHandlerContext, ChatHandlerInput, handle_chat_core};

use super::conversion::{convert_agent_config, convert_attachments, convert_model_provider};
use super::locale::locale_from_grpc_request;

pub async fn chat(
    app_state: &Arc<AppState>,
    request: Request<GrpcChatRequest>,
) -> Result<Response<GrpcChatResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    let req = request.into_inner();

    let model_config_debug = req
        .model_config
        .as_ref()
        .map(shared_types::MaskedModelConfig);

    info!(
        "🚀 [gRPC] Chat request: project_id={}, session_id={}, prompt_len={}, agent_config={:?}, model_config={:?}, service_type={:?}, user_id={:?}, has_attachments={}, has_data_source={}",
        req.project_id,
        req.session_id,
        req.prompt.len(),
        req.agent_config,
        model_config_debug,
        req.service_type,
        req.user_id,
        !req.attachments.is_empty(),
        !req.data_source_attachments.is_empty()
    );

    if req.prompt.trim().is_empty() {
        return Err(Status::invalid_argument("prompt field cannot be empty"));
    }

    let project_id = if req.project_id.is_empty() {
        uuid::Uuid::new_v4().to_string().replace("-", "")
    } else {
        req.project_id.clone()
    };

    let session_id = if req.session_id.is_empty() {
        None
    } else {
        Some(req.session_id.clone())
    };

    let request_id = req
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace("-", ""));

    let service_type = req
        .service_type
        .as_ref()
        .and_then(|st| match st.as_str() {
            "ComputerAgentRunner" => Some(shared_types::ServiceType::ComputerAgentRunner),
            "RCoder" => Some(shared_types::ServiceType::RCoder),
            _ => {
                warn!("[gRPC] Invalid service_type: {}, using default RCoder", st);
                None
            }
        })
        .unwrap_or(shared_types::ServiceType::RCoder);

    let project_dir = match service_type {
        shared_types::ServiceType::ComputerAgentRunner => {
            std::path::PathBuf::from("/home/user").join(&project_id)
        }
        shared_types::ServiceType::RCoder => {
            let tenant_id = std::env::var("TENANT_ID").ok();
            let space_id = std::env::var("SPACE_ID").ok();
            match (tenant_id, space_id) {
                (Some(tid), Some(sid)) => std::path::PathBuf::from("./project_workspace")
                    .join(&tid)
                    .join(&sid)
                    .join(&project_id),
                _ => std::path::PathBuf::from("./project_workspace").join(&project_id),
            }
        }
    };

    let agent_config_override = req.agent_config.map(convert_agent_config).transpose()?;

    let input = ChatHandlerInput {
        project_id,
        project_dir,
        session_id,
        prompt: req.prompt,
        request_id,
        attachments: convert_attachments(req.attachments),
        data_source_attachments: req.data_source_attachments,
        model_config: req.model_config.map(convert_model_provider),
        service_type,
        user_id: req.user_id,
        agent_config_override,
        system_prompt_override: req.system_prompt,
        user_prompt_template_override: req.user_prompt,
    };

    let context = ChatHandlerContext {
        agent_session_service: app_state.agent_session_service.clone(),
        shared_api_key_manager: app_state.shared_api_key_manager.clone(),
        project_uuid_map: app_state.project_uuid_map.clone(),
    };

    let output =
        shared_types::scope_request_locale(locale, handle_chat_core(input, &context)).await;

    let grpc_response = GrpcChatResponse {
        project_id: output.project_id,
        session_id: output.session_id,
        success: output.success,
        error: output.error,
        error_code: output.error_code,
        request_id: output.request_id,
        need_fallback: output.need_fallback,
        fallback_reason: output.fallback_reason,
        reloaded: output.reloaded,
    };

    info!("[gRPC] Chat completed: success={}", grpc_response.success);

    Ok(Response::new(grpc_response))
}

//! Chat RPC 实现

use std::sync::Arc;

use shared_types::grpc::{ChatRequest as GrpcChatRequest, ChatResponse as GrpcChatResponse};
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
        .and_then(|st| match st.parse::<shared_types::ServiceType>() {
            Ok(t) => Some(t),
            Err(e) => {
                warn!(
                    "[gRPC] Invalid service_type: {}, using default WebAgentRunner. Error: {}",
                    st, e
                );
                None
            }
        })
        .unwrap_or(shared_types::ServiceType::WebAgentRunner);

    let app_id = req.app_id.clone().filter(|s| !s.is_empty());

    // Fail Fast：app_id 是 UserappBuilder 场景的定位键（与 project_id 语义独立，
    // 不做回落——缺失即调用方契约错误，显式拒绝）
    validate_userapp_app_id(&service_type, app_id.as_deref())?;

    // UserappBuilder 不消费 agent_work_dir（computer 场景专用概念）。网关已消毒，
    // 这里对绕过网关直连/漏改的调用留痕，不静默（值等于 app_id 时视为无害冗余）
    if matches!(service_type, shared_types::ServiceType::UserappBuilder)
        && let Some(raw) = req.agent_work_dir.as_deref()
        && !raw.is_empty()
        && Some(raw) != app_id.as_deref()
    {
        warn!(
            "[gRPC] UserappBuilder ignores agent_work_dir (computer-only concept): got={raw}, app_id={}",
            app_id.as_deref().unwrap_or_default()
        );
    }

    // 实际用于工作目录拼接的标识符（UserappBuilder=app_id；其余=work_dir_id，
    // 即 agent_work_dir 优先 project_id 的原语义）。纵深防御：即使 HTTP/网关
    // 入口已校验，gRPC 入口也应校验。校验按 service_type 分派：Computer 放行
    // 绝对路径形态（常规项目场景），Web 等其余显式拒绝（fail-fast）
    let (dir_key, dir_key_name) = match service_type {
        shared_types::ServiceType::UserappBuilder => (app_id.clone().unwrap_or_default(), "app_id"),
        _ => (
            req.agent_work_dir
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| project_id.clone()),
            "agent_work_dir",
        ),
    };
    let dir_key_validation = match service_type {
        shared_types::ServiceType::UserappBuilder => {
            shared_types::validate_identifier(&dir_key, dir_key_name)
        }
        _ => shared_types::validate_agent_work_dir_for_service(&service_type, &dir_key),
    };
    if let Err(e) = dir_key_validation {
        return Err(Status::invalid_argument(e));
    }

    let project_dir = resolve_project_dir(
        &service_type,
        &project_id,
        app_id.as_deref(),
        req.agent_work_dir.as_deref(),
        &crate::userapp_env::userapp_workspace_dir(),
    );

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
        is_devcomputer: req.is_devcomputer, // 🆕 从 gRPC 请求中读取
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
        agent_version: output.agent_version,
    };

    info!("[gRPC] Chat completed: success={}", grpc_response.success);

    Ok(Response::new(grpc_response))
}

/// UserappBuilder 场景 app_id 必填校验（Fail Fast：app_id 与 project_id 是语义
/// 独立的字段——定位键缺失即调用方契约错误，不做 project_id 回落）。
fn validate_userapp_app_id(
    service_type: &shared_types::ServiceType,
    app_id: Option<&str>,
) -> Result<(), Status> {
    if matches!(service_type, shared_types::ServiceType::UserappBuilder) && app_id.is_none() {
        return Err(Status::invalid_argument(
            "app_id is required for UserappBuilder chat",
        ));
    }
    Ok(())
}

/// 按 service_type 解析 chat 工作目录（纯函数，env 读取参数化便于测试）。
///
/// - UserappBuilder：键 = app_id（入口已校验必填）。不消费 agent_work_dir
///   （computer 场景专用概念，Java 曾借它渗入会话 ID 导致 agent 落错目录）；
///   根 = userapp_root（env USERAPP_WORKSPACE_DIR 缺省 /home/user，见
///   userapp_env.rs——挂载压平契约，勿用沙箱视角的 USERAPP_WORKSPACE_ROOT）
/// - ComputerAgentRunner：work_dir_id（agent_work_dir 优先，原语义）两形态——
///   单段目录名 → `/home/user + work_dir_id`；绝对路径（常规项目场景，Java 传
///   子容器内 `/home/user/{projectType}/{projectId}`）→ 原样作为工作目录。
///   绝对形态仅 Computer 支持（入口 `validate_agent_work_dir_for_service`
///   已挡其余 service_type）
/// - WebAgentRunner/Userapp：./project_workspace + tenant/space 分支（原语义，
///   仅单段 work_dir_id）
fn resolve_project_dir(
    service_type: &shared_types::ServiceType,
    project_id: &str,
    app_id: Option<&str>,
    agent_work_dir: Option<&str>,
    userapp_root: &str,
) -> std::path::PathBuf {
    // computer/web 场景：work_dir_id 优先 agent_work_dir（proto 语义：自定义
    // 目录名或绝对路径，替代 project_id 参与拼接）
    let work_dir_id = agent_work_dir
        .filter(|s| !s.is_empty())
        .unwrap_or(project_id);
    match service_type {
        shared_types::ServiceType::ComputerAgentRunner => {
            // 绝对路径形态：显式分派为原样使用，而非依赖 join 对绝对参数的
            // 隐式前缀替换语义（跨平台字符串判定见 is_absolute_path_like）
            if shared_types::is_absolute_path_like(work_dir_id) {
                std::path::PathBuf::from(work_dir_id)
            } else {
                std::path::PathBuf::from("/home/user").join(work_dir_id)
            }
        }
        shared_types::ServiceType::UserappBuilder => {
            std::path::PathBuf::from(userapp_root).join(app_id.unwrap_or_default())
        }
        // Userapp 不由 agent_runner 托管；WebAgentRunner 走 project_workspace 路径
        shared_types::ServiceType::WebAgentRunner | shared_types::ServiceType::Userapp => {
            let tenant_id = std::env::var("TENANT_ID").ok();
            let space_id = std::env::var("SPACE_ID").ok();
            match (tenant_id, space_id) {
                (Some(tid), Some(sid)) => std::path::PathBuf::from("./project_workspace")
                    .join(tid)
                    .join(sid)
                    .join(work_dir_id),
                _ => std::path::PathBuf::from("./project_workspace").join(work_dir_id),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::ServiceType;

    #[test]
    fn userapp_builder_uses_app_id_field() {
        // 新契约主路径：定位键=app_id，agent_work_dir（Java 会话 ID 形态）被忽略，
        // 根=注入的 userapp_root（builder 容器内即 /home/user，PVC 挂载点父目录）
        let dir = resolve_project_dir(
            &ServiceType::UserappBuilder,
            "13",
            Some("13"),
            Some("1561845"),
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("/tmp/ws/13"));
    }

    #[test]
    fn userapp_builder_without_agent_work_dir() {
        let dir = resolve_project_dir(
            &ServiceType::UserappBuilder,
            "13",
            Some("13"),
            None,
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("/tmp/ws/13"));
    }

    #[test]
    fn userapp_builder_missing_app_id_rejected() {
        // Fail Fast：app_id 与 project_id 语义独立，缺失即拒绝，无回落
        let err = validate_userapp_app_id(&ServiceType::UserappBuilder, None).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("app_id is required"));
    }

    #[test]
    fn other_service_types_do_not_require_app_id() {
        assert!(validate_userapp_app_id(&ServiceType::ComputerAgentRunner, None).is_ok());
        assert!(validate_userapp_app_id(&ServiceType::WebAgentRunner, None).is_ok());
    }

    #[test]
    fn computer_agent_runner_keeps_agent_work_dir_priority() {
        // computer 语义不变：agent_work_dir 优先于 project_id，app_id 不参与
        let dir = resolve_project_dir(
            &ServiceType::ComputerAgentRunner,
            "13",
            Some("13"),
            Some("1561845"),
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("/home/user/1561845"));
    }

    #[test]
    fn computer_agent_runner_falls_back_to_project_id() {
        let dir = resolve_project_dir(
            &ServiceType::ComputerAgentRunner,
            "13",
            None,
            None,
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("/home/user/13"));
    }

    #[test]
    fn computer_agent_runner_absolute_path_verbatim() {
        // 常规项目场景：Java 传子容器内绝对路径 /home/user/{projectType}/{projectId}，
        // 原样作为工作目录。显式分派语义——join 对绝对参数本就隐式替换前缀，
        // 改造前后行为相同，此用例固化「显式化」而非行为变化
        let dir = resolve_project_dir(
            &ServiceType::ComputerAgentRunner,
            "13",
            None,
            Some("/home/user/web/proj_1"),
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("/home/user/web/proj_1"));
    }

    #[test]
    fn computer_agent_runner_windows_drive_path_verbatim() {
        // 多平台：Windows 盘符形态（未来本机 agent_runner 场景）同样原样使用
        let dir = resolve_project_dir(
            &ServiceType::ComputerAgentRunner,
            "13",
            None,
            Some("C:/Users/dev/proj"),
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("C:/Users/dev/proj"));
    }

    #[test]
    fn computer_agent_runner_windows_verbatim_prefix_verbatim() {
        // Windows verbatim 扩展长度路径（\\?\ 前缀，Electron/Win32 API 长路径产物）：
        // 校验层 de-verbatim 判定合法后，raw 值原样作为工作目录——
        // PathBuf::from 在 Windows std 本就是合法 verbatim 绝对路径
        let raw = r"\\?\C:\Users\dev\proj";
        let dir = resolve_project_dir(
            &ServiceType::ComputerAgentRunner,
            "13",
            None,
            Some(raw),
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from(raw));
    }

    #[test]
    fn web_agent_runner_semantics_unchanged() {
        // 单级：./project_workspace/{work_dir_id}（测试环境无 TENANT_ID/SPACE_ID，
        // 有则跳过避免 flaky；app_id 不参与 web 路径）
        if std::env::var("TENANT_ID").is_ok() || std::env::var("SPACE_ID").is_ok() {
            return;
        }
        let dir = resolve_project_dir(
            &ServiceType::WebAgentRunner,
            "p1",
            Some("ignored"),
            None,
            "/tmp/ws",
        );
        assert_eq!(dir, std::path::PathBuf::from("./project_workspace/p1"));
    }
}

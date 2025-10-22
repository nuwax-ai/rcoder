//! Agent执行任务的SSE通知处理器
//!
//! 使用 Pingora 透明代理处理 SSE 消息

use crate::{AgentStatus, AppError, HttpResult, UnifiedSessionMessage};
use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::{
    Stream,
    stream::{self},
};
use serde::{Deserialize, Serialize};
use shared_types::ProjectAndAgentInfo;
use std::convert::Infallible;
use tracing::{error, info};
use utoipa::{IntoParams, ToSchema};
/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// SSE 事件响应结构
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionUpdateEvent {
    /// 事件类型
    #[schema(example = "prompt_start")]
    pub event_type: String,
    /// 会话ID
    #[schema(example = "session456")]
    pub session_id: String,
    /// 统一会话消息
    pub message: UnifiedSessionMessage,
}

/// 使用 Pingora 透明代理检查容器存在性并代理 SSE 请求
async fn check_container_and_proxy_sse(session_id: &str) -> Result<(String, String), AppError> {
    info!("🔍 [SSE_PROXY] 检查容器可用性: session_id={}", session_id);

    // 查找对应的容器
    if let Some((project_id, agent_info)) = find_container_by_session_id(session_id) {
        info!(
            "✅ [SSE_PROXY] 找到对应容器: project_id={}, session_id={}",
            project_id, session_id
        );

        // 获取容器的服务地址
        let container_service_url = get_container_service_url(&project_id, &agent_info).await?;

        // 构建 Pingora 透明代理目标URL
        let proxy_target_url = format!(
            "{}/agent/agent/progress?session_id={}",
            container_service_url, session_id
        );

        info!(
            "🚀 [SSE_PROXY] 容器可用，将通过 Pingora 代理到: {}",
            proxy_target_url
        );

        Ok((proxy_target_url, project_id))
    } else {
        error!(
            "❌ [SSE_PROXY] 未找到对应容器，无法建立 SSE 连接: session_id={}",
            session_id
        );
        Err(AppError::internal_server_error(&format!(
            "未找到 session_id={} 对应的活跃容器",
            session_id
        )))
    }
}

/// Pingora 透明代理 SSE 流处理器
async fn pingora_proxy_sse_stream(
    session_id: String,
) -> Result<impl Stream<Item = Result<Event, Infallible>>, AppError> {
    info!(
        "🌐 [PINGORA_PROXY] 开始 Pingora SSE 代理: session_id={}",
        session_id
    );

    // 检查容器存在性并获取代理目标
    let (proxy_target_url, project_id) =
        check_container_and_proxy_sse(&session_id).await?;

    // 这里应该集成 Pingora 的透明代理功能
    // 由于当前还没有 Pingora 集成，我们先实现一个简化版本
    info!(
        "📡 [PINGORA_PROXY] 代理 SSE 请求到容器: target_url={}, project_id={}",
        proxy_target_url, project_id
    );
    //todo: 集成 Pingora 的透明代理功能

    // 创建连接事件
    let connection_event = Event::default().event("connected").data(format!(
        r#"{{"type":"connected","message":"Pingora proxy established for session {}"}}"#,
        session_id
    ));

    // 创建代理就绪事件
    let proxy_ready_event = Event::default().event("proxy_ready").data(format!(
        r#"{{"type":"proxy_ready","target":"{}","project_id":"{}"}}"#,
        proxy_target_url, project_id
    ));

    // 创建事件流
    let event_stream = stream::iter(vec![Ok(connection_event), Ok(proxy_ready_event)]);

    Ok(event_stream)
}

/// 解析容器返回的SSE事件数据
fn parse_container_sse_event(chunk: &[u8]) -> Option<ContainerSseEvent> {
    // 简化的事件解析逻辑
    let chunk_str = String::from_utf8_lossy(chunk);

    // 尝试解析JSON格式的SSE数据
    if let Ok(event_data) = serde_json::from_str::<ContainerSseEventData>(&chunk_str) {
        return Some(ContainerSseEvent {
            event_type: event_data.event_type,
            message: event_data.message,
        });
    }

    None
}

/// 容器SSE事件数据格式
#[derive(Debug, Deserialize)]
struct ContainerSseEventData {
    pub event_type: String,
    pub message: UnifiedSessionMessage,
}

/// 容器SSE事件
#[derive(Debug)]
struct ContainerSseEvent {
    pub event_type: String,
    pub message: UnifiedSessionMessage,
}

/// 检查或创建容器（用于SSE请求）
async fn ensure_container_exists_for_sse(session_id: &str) -> Result<(String, String), AppError> {
    // 对于SSE请求，我们优先查找现有的容器
    // 如果找不到，可能需要创建一个容器来处理这个session
    if let Some((project_id, agent_info)) = find_container_by_session_id(session_id) {
        info!(
            "🔍 [SSE_FORWARD] 找到现有容器: project_id={}, session_id={}",
            project_id, session_id
        );

        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config()
                .await
                .map_err(|e| {
                    error!("❌ [SSE_FORWARD] 创建 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?,
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(&project_id);
        if let Some(container_info) = container_info {
            let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(
                &docker_manager,
                &container_info.container_id,
                container_info.assigned_port,
            )
            .await
            .map_err(|e| {
                error!("❌ [SSE_FORWARD] 获取容器 IP 失败: {}", e);
                AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
            })?;

            info!("✅ [SSE_FORWARD] 获取容器服务 URL: {}", server_url);
            Ok((server_url, project_id))
        } else {
            Err(AppError::internal_server_error("未找到容器信息"))
        }
    } else {
        // 没有找到现有容器，为SSE请求创建一个临时容器
        info!(
            "🏗️ [SSE_FORWARD] 未找到容器，为SSE请求创建临时容器: session_id={}",
            session_id
        );

        // 使用默认配置创建临时容器用于SSE
        let chat_prompt = shared_types::ChatPromptBuilder::default()
            .project_id("sse_temp".to_string())
            .session_id(session_id.to_string())
            .prompt("sse_request".to_string())
            .build()
            .map_err(|e| {
                error!("❌ [SSE_FORWARD] 构建 ChatPrompt 失败: {}", e);
                AppError::internal_server_error(&format!("构建 ChatPrompt 失败: {}", e))
            })?;

        // 创建临时容器
        create_container_for_sse(&chat_prompt, None).await?;

        // 返回错误，因为SSE不应该需要创建新容器
        Err(AppError::internal_server_error(
            "SSE request requires existing session",
        ))
    }
}

/// 获取容器服务地址
async fn get_container_service_url(
    project_id: &str,
    _agent_info: &ProjectAndAgentInfo,
) -> Result<String, AppError> {
    info!("🔍 [CONTAINER] 获取容器服务地址: project_id={}", project_id);

    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER] 创建 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 获取容器信息
    let container_info = docker_manager.get_container_info(project_id);
    if let Some(container_info) = container_info {
        let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(
            &docker_manager,
            &container_info.container_id,
            container_info.assigned_port,
        )
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER] 获取容器 IP 失败: {}", e);
            AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
        })?;

        info!("✅ [CONTAINER] 获取容器服务 URL: {}", server_url);
        Ok(server_url)
    } else {
        Err(AppError::internal_server_error("未找到容器信息"))
    }
}

/// 根据session_id查找对应的容器
fn find_container_by_session_id(
    session_id: &str,
) -> Option<(String, std::sync::Arc<ProjectAndAgentInfo>)> {
    use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;

    for entry in PROJECT_AND_AGENT_INFO_MAP.iter() {
        let agent_info = entry.value();
        if agent_info.session_id.to_string() == session_id {
            return Some((entry.key().clone(), std::sync::Arc::new(agent_info.clone())));
        }
    }
    None
}

/// 为SSE请求创建容器
async fn create_container_for_sse(
    chat_prompt: &shared_types::ChatPrompt,
    _model_provider: Option<shared_types::ModelProviderConfig>,
) -> Result<(), AppError> {
    let project_id = &chat_prompt.project_id;
    info!(
        "🏗️ [SSE_FORWARD] 开始为SSE请求创建容器: project_id={}",
        project_id
    );

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!(
                    "❌ [SSE_FORWARD] 创建 DockerManager 失败: project_id={}, error={}",
                    project_id, e
                );
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    let connection_info =
        crate::proxy_agent::docker_container_agent::start_docker_container_agent_service(
            chat_prompt.clone(),
            None, // SSE请求不需要特定的 model provider
            docker_manager,
        )
        .await
        .map_err(|e| {
            error!(
                "❌ [SSE_FORWARD] 创建容器失败: project_id={}, error={}",
                project_id, e
            );
            AppError::internal_server_error(&format!("创建容器失败: {}", e))
        })?;

    info!(
        "✅ [SSE_FORWARD] 容器创建成功: project_id={}, session_id={}",
        project_id, connection_info.session_id
    );

    // 创建生命周期守卫并存储到 MAP 中
    let project_and_agent_info = shared_types::ProjectAndAgentInfo {
        project_id: project_id.clone(),
        session_id: connection_info.session_id.clone(),
        prompt_tx: connection_info.prompt_tx.clone(),
        cancel_tx: connection_info.cancel_tx.clone(),
        model_provider: None,
        request_id: chat_prompt.request_id.clone(),
        status: AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
    };

    // 存储到全局 MAP
    crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP
        .insert(project_id.clone(), project_and_agent_info);

    // 建立 project_id -> session_id 映射
    let session_id_str = connection_info.session_id.to_string();
    let cleared_old =
        crate::service::session_cache::ensure_project_session(project_id, &session_id_str).await;
    if cleared_old > 0 {
        info!(
            "🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
            project_id, cleared_old
        );
    }

    info!(
        "✅ [SSE_FORWARD] 容器创建完成并已注册: project_id={}",
        project_id
    );
    Ok(())
}
/// 建立SSE连接，实时推送该session的SessionUpdate消息
///
/// 通过Server-Sent Events (SSE)协议实时推送AI代理执行进度和状态更新
///
/// ## 📨 支持的消息类型
///
/// 返回的UnifiedSessionMessage包含以下主要类型：
///
/// 1. **SessionPromptStart** - 用户发送prompt开始通知
/// 2. **SessionPromptEnd** - Agent执行结束通知（包含5种停止原因：EndTurn, MaxTokens, MaxTurnRequests, Refusal, Cancelled）
/// 3. **AgentSessionUpdate** - Agent执行过程中的更新通知（包含8种子类型：UserMessageChunk, AgentMessageChunk, AgentThoughtChunk, ToolCall, ToolCallUpdate, Plan, AvailableCommandsUpdate, CurrentModeUpdate）
/// 4. **Heartbeat** - SSE连接心跳消息
///
/// ## 🔄 事件类型映射
///
/// SSE消息事件名称与UnifiedSessionMessage类型的映射关系：
/// - `prompt_start` → SessionMessageType::SessionPromptStart
/// - `prompt_end` → SessionMessageType::SessionPromptEnd
/// - `user_message_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "user_message_chunk")
/// - `agent_message_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "agent_message_chunk")
/// - `agent_thought_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "agent_thought_chunk")
/// - `tool_call` → SessionMessageType::AgentSessionUpdate (sub_type: "tool_call")
/// - `tool_call_update` → SessionMessageType::AgentSessionUpdate (sub_type: "tool_call_update")
/// - `plan` → SessionMessageType::AgentSessionUpdate (sub_type: "plan")
/// - `available_commands_update` → SessionMessageType::AgentSessionUpdate (sub_type: "available_commands_update")
/// - `current_mode_update` → SessionMessageType::AgentSessionUpdate (sub_type: "current_mode_update")
/// - `heartbeat` → SessionMessageType::Heartbeat
///
/// ## 💡 前端集成建议
///
/// 前端开发者需要：
/// 1. 建立SSE连接并监听不同的事件类型
/// 2. 根据message_type和sub_type处理不同的消息场景
/// 3. 实现心跳检测机制确保连接活跃
/// 4. 处理错误情况并实现自动重连
///
/// 建立SSE连接，将请求转发到容器内的 agent_runner 服务
///
/// 详细的JSON格式示例请参考UnifiedSessionMessage结构体定义和具体字段的Schema说明。
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        SessionNotificationParams
    ),
    responses(
        (
            status = 200,
            description = "成功建立SSE连接，开始推送实时更新。连接建立后，将实时推送该会话的所有状态更新消息。",
            content_type = "text/event-stream",
            body = UnifiedSessionMessage,
            examples(
                ("SessionPromptStart" = (summary = "用户请求开始", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptStart",
                    "sub_type": "prompt_start",
                    "data": {
                        "type": "prompt_start",
                        "prompt": "帮我写一个Rust的Hello World程序",
                        "attachments": [
                            {
                                "type": "text",
                                "content": "请包含详细的注释说明"
                            }
                        ],
                        "user_id": "user123",
                        "project_id": "test_project"
                    },
                    "timestamp": "2023-12-01T10:30:00Z"
                }))),
                ("SessionPromptEnd_EndTurn" = (summary = "正常结束", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptEnd",
                    "sub_type": "end_turn",
                    "data": {
                        "stop_reason": "end_turn",
                        "message": "任务完成：成功创建Hello World程序",
                        "tool_calls": [
                            {
                                "name": "write_file",
                                "status": "completed",
                                "duration_ms": 150
                            }
                        ],
                        "total_tokens": 245,
                        "duration_ms": 3200
                    },
                    "timestamp": "2023-12-01T10:30:05Z"
                }))),
                ("SessionPromptEnd_MaxTokens" = (summary = "令牌限制", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptEnd",
                    "sub_type": "max_tokens",
                    "data": {
                        "stop_reason": "max_tokens",
                        "message": "已达到最大令牌数限制",
                        "error_message": "Token limit exceeded: max_tokens=4000, used_tokens=4025",
                        "suggestion": "请简化请求或分段处理"
                    },
                    "timestamp": "2023-12-01T10:30:45Z"
                }))),
                ("UserMessageChunk" = (summary = "用户消息块", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "user_message_chunk",
                    "data": {
                        "content": {
                            "type": "text",
                            "text": "请帮我创建一个Rust项目，包含Hello World程序",
                            "annotations": null,
                            "meta": null
                        }
                    },
                    "timestamp": "2023-12-01T10:30:00Z"
                }))),
                ("AgentMessageChunk" = (summary = "Agent响应消息块", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "agent_message_chunk",
                    "data": {
                        "content": {
                            "type": "text",
                            "text": "当然可以！以下是一个简单的Rust Hello World程序：\\n\\n```rust\\nfn main() {\\n    println!(\\\"Hello, World!\\\");\\n}\\n```\\n\\n要运行这个程序，您需要：\\n1. 安装Rust环境\\n2. 创建项目：`cargo new hello_world`\\n3. 替换src/main.rs内容\\n4. 运行：`cargo run`",
                            "annotations": null,
                            "meta": null
                        },
                        "is_final": false
                    },
                    "timestamp": "2023-12-01T10:30:02Z"
                }))),
                ("AgentThoughtChunk" = (summary = "Agent思考过程", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "agent_thought_chunk",
                    "data": {
                        "content": {
                            "type": "text",
                            "text": "用户要求创建Rust Hello World程序。我需要：1) 创建项目结构，2) 编写main.rs文件，3) 提供运行说明。这是一个基础任务，可以直接完成。",
                            "annotations": null,
                            "meta": null
                        },
                        "confidence": 0.95
                    },
                    "timestamp": "2023-12-01T10:30:01Z"
                }))),
                ("ToolCall" = (summary = "工具调用", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "tool_call",
                    "data": {
                        "toolCallId": "call_123456",
                        "title": "创建文件",
                        "kind": "file",
                        "status": "pending",
                        "content": [],
                        "locations": [
                            {
                                "path": "src/main.rs",
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 3, "character": 1 }
                                }
                            }
                        ],
                        "raw_input": {
                            "path": "src/main.rs",
                            "content": "fn main() {\n    println!(\"Hello, World!\");\n}"
                        },
                        "raw_output": null
                    },
                    "timestamp": "2023-12-01T10:30:03Z"
                }))),
                ("ToolCallUpdate" = (summary = "工具调用状态更新", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "tool_call_update",
                    "data": {
                        "toolCallId": "call_123456",
                        "status": "success",
                        "result": {
                            "path": "src/main.rs",
                            "content_length": 48,
                            "created": true,
                            "checksum": "abc123"
                        }
                    },
                    "timestamp": "2023-12-01T10:30:04Z"
                }))),
                ("Plan" = (summary = "执行计划", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "plan",
                    "data": {
                        "entries": [
                            {
                                "content": "创建项目目录结构",
                                "priority": "high",
                                "status": "completed",
                                "meta": null
                            },
                            {
                                "content": "编写main.rs文件",
                                "priority": "high",
                                "status": "in_progress",
                                "meta": null
                            },
                            {
                                "content": "验证程序运行",
                                "priority": "medium",
                                "status": "pending",
                                "meta": null
                            }
                        ],
                        "meta": null
                    },
                    "timestamp": "2023-12-01T10:30:01Z"
                }))),
                ("AvailableCommandsUpdate" = (summary = "可用命令更新", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "available_commands_update",
                    "data": {
                        "available_commands": [
                            {
                                "name": "write_file",
                                "description": "写入文件内容",
                                "input": {
                                    "hint": "请输入文件路径和内容"
                                },
                                "meta": null
                            },
                            {
                                "name": "read_file",
                                "description": "读取文件内容",
                                "input": {
                                    "hint": "请输入文件路径"
                                },
                                "meta": null
                            },
                            {
                                "name": "run_command",
                                "description": "执行系统命令",
                                "input": {
                                    "hint": "请输入要执行的命令"
                                },
                                "meta": null
                            }
                        ]
                    },
                    "timestamp": "2023-12-01T10:30:00Z"
                }))),
                ("CurrentModeUpdate" = (summary = "当前模式更新", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "current_mode_update",
                    "data": {
                        "current_mode_id": "code",
                        "available_modes": [
                            {
                                "id": "ask",
                                "name": "问答模式",
                                "description": "回答问题和提供建议"
                            },
                            {
                                "id": "code",
                                "name": "编程模式",
                                "description": "编写和修改代码"
                            },
                            {
                                "id": "architect",
                                "name": "架构模式",
                                "description": "设计和规划项目架构"
                            }
                        ]
                    },
                    "timestamp": "2023-12-01T10:30:00Z"
                }))),
                ("Heartbeat" = (summary = "心跳消息", value = json!({
                    "session_id": "session456",
                    "message_type": "Heartbeat",
                    "sub_type": "ping",
                    "data": {
                        "type": "heartbeat",
                        "message": "keep-alive",
                        "timestamp": "2023-12-01T10:31:00Z"
                    },
                    "timestamp": "2023-12-01T10:31:00Z"
                })))
            )
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INVALID_PARAMS",
                    "message": "Invalid session_id parameter"
                }
            })
        ),
        (
            status = 500,
            description = "转发SSE请求失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "SSE_FAILED",
                    "message": "Failed to forward SSE request to container"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "转发Agent会话通知",
    description = "建立SSE连接并将事件请求转发到容器内的 agent_runner/agent/progress 接口，通过容器获取实时会话更新事件。"
)]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    info!(
        "🌐 [PINGORA_PROXY] 收到SSE连接请求: session_id={:?}",
        params.session_id
    );

    // 使用 Pingora 透明代理架构处理 SSE 请求
    let session_id = params.session_id.clone();
    let event_stream = pingora_proxy_sse_stream(session_id.clone()).await?;
    info!(
        "✅ [PINGORA_PROXY] SSE 代理连接建立: session_id={}",
        session_id
    );
    Ok(Sse::new(event_stream))
}

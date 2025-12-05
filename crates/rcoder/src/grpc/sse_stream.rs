//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use shared_types::grpc::{ProgressRequest, agent_service_client::AgentServiceClient};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

/// 创建基于 gRPC 的 SSE 代理流
///
/// 通过 gRPC `SubscribeProgress` 方法订阅 agent_runner 的进度事件，
/// 并将事件转换为 SSE 格式返回
pub async fn create_grpc_sse_stream(
    grpc_addr: String,
    session_id: String,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let session_id_clone = session_id.clone();

    // 在后台任务中处理 gRPC 流
    tokio::spawn(async move {
        info!(
            "🔗 [gRPC_SSE] 开始连接 agent_runner gRPC: addr={}, session_id={}",
            grpc_addr, session_id_clone
        );

        // 建立 gRPC 连接
        let endpoint = format!("http://{}", grpc_addr);
        match Channel::from_shared(endpoint.clone()) {
            Ok(channel_builder) => {
                match channel_builder
                    .connect_timeout(std::time::Duration::from_secs(
                        shared_types::GRPC_CONNECT_TIMEOUT_SECS,
                    ))
                    .timeout(std::time::Duration::from_secs(
                        shared_types::GRPC_REQUEST_TIMEOUT_SECS,
                    ))
                    .connect()
                    .await
                {
                    Ok(channel) => {
                        let mut client = AgentServiceClient::new(channel);

                        // 发送 SubscribeProgress 请求
                        let request = tonic::Request::new(ProgressRequest {
                            session_id: session_id_clone.clone(),
                        });

                        match client.subscribe_progress(request).await {
                            Ok(response) => {
                                info!(
                                    "✅ [gRPC_SSE] 成功建立 SubscribeProgress 流: session_id={}",
                                    session_id_clone
                                );

                                let mut stream = response.into_inner();

                                // 持续接收 gRPC 流中的事件
                                while let Ok(Some(progress_event)) = stream.message().await {
                                    debug!(
                                        "📨 [gRPC_SSE] 收到进度事件: session_id={}, timestamp={}",
                                        session_id_clone, progress_event.timestamp
                                    );

                                    // 将 ProgressEvent 转换为 SSE Event
                                    let sse_event = progress_event_to_sse(&progress_event);

                                    if tx.send(Ok(sse_event)).await.is_err() {
                                        warn!(
                                            "⚠️ [gRPC_SSE] 客户端已断开连接: session_id={}",
                                            session_id_clone
                                        );
                                        break;
                                    }
                                }

                                info!("🔚 [gRPC_SSE] gRPC 流结束: session_id={}", session_id_clone);
                            }
                            Err(e) => {
                                error!(
                                    "❌ [gRPC_SSE] SubscribeProgress 调用失败: session_id={}, error={}",
                                    session_id_clone, e
                                );

                                // 发送错误事件
                                let error_event = axum::response::sse::Event::default()
                                    .event("error")
                                    .data(format!("gRPC 流订阅失败: {}", e));
                                let _ = tx.send(Ok(error_event)).await;
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "❌ [gRPC_SSE] 无法连接到 gRPC 服务: addr={}, error={}",
                            endpoint, e
                        );

                        let error_event = axum::response::sse::Event::default()
                            .event("error")
                            .data(format!("gRPC 连接失败: {}", e));
                        let _ = tx.send(Ok(error_event)).await;
                    }
                }
            }
            Err(e) => {
                error!("❌ [gRPC_SSE] 无效的 gRPC 地址: {}, error={}", grpc_addr, e);

                let error_event = axum::response::sse::Event::default()
                    .event("error")
                    .data(format!("无效的 gRPC 地址: {}", e));
                let _ = tx.send(Ok(error_event)).await;
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// 将 gRPC ProgressEvent 转换为 SSE Event
///
/// 使用 oneof event 字段进行类型安全的转换
fn progress_event_to_sse(event: &shared_types::grpc::ProgressEvent) -> axum::response::sse::Event {
    use shared_types::grpc::progress_event::Event;

    // 处理 oneof event 字段
    if let Some(ref event_data) = event.event {
        match event_data {
            Event::Log(log) => {
                let data = serde_json::json!({
                    "level": log.level,
                    "message": log.message
                });
                axum::response::sse::Event::default()
                    .event("log")
                    .data(data.to_string())
            }

            Event::Thinking(thinking) => {
                let data = serde_json::json!({
                    "content": thinking.content,
                    "is_complete": thinking.is_complete
                });
                axum::response::sse::Event::default()
                    .event("thinking")
                    .data(data.to_string())
            }

            Event::Chunk(chunk) => {
                let data = serde_json::json!({
                    "content": chunk.content,
                    "index": chunk.index
                });
                axum::response::sse::Event::default()
                    .event("chunk")
                    .data(data.to_string())
            }

            Event::Completion(completion) => {
                let data = serde_json::json!({
                    "result": completion.result,
                    "total_tokens": completion.total_tokens,
                    "duration_ms": completion.duration_ms
                });
                axum::response::sse::Event::default()
                    .event("completion")
                    .data(data.to_string())
            }

            Event::Error(error) => {
                let data = serde_json::json!({
                    "error_code": error.error_code,
                    "error_message": error.error_message,
                    "stack_trace": error.stack_trace
                });
                axum::response::sse::Event::default()
                    .event("error")
                    .data(data.to_string())
            }

            Event::AskConfirmation(ask) => {
                let data = serde_json::json!({
                    "message": ask.message,
                    "options": ask.options,
                    "default_option": ask.default_option
                });
                axum::response::sse::Event::default()
                    .event("ask_confirmation")
                    .data(data.to_string())
            }

            Event::ProgressNotification(progress) => {
                let data = serde_json::json!({
                    "status": progress.status,
                    "percentage": progress.percentage,
                    "details": progress.details
                });
                axum::response::sse::Event::default()
                    .event("progress_notification")
                    .data(data.to_string())
            }

            Event::ToolUse(tool) => {
                let data = serde_json::json!({
                    "tool_name": tool.tool_name,
                    "tool_input": tool.tool_input,
                    "tool_output": tool.tool_output,
                    "is_error": tool.is_error
                });
                axum::response::sse::Event::default()
                    .event("tool_use")
                    .data(data.to_string())
            }
        }
    } else {
        // 空事件，发送心跳
        axum::response::sse::Event::default().comment("heartbeat")
    }
}

/// 获取容器的 gRPC 地址
///
/// 返回格式: `{container_ip}:{grpc_port}`
/// 默认 gRPC 端口为 50051
pub async fn get_container_grpc_addr(project_id: &str, grpc_port: u16) -> anyhow::Result<String> {
    info!(
        "🔍 [CONTAINER] 获取容器 gRPC 地址: project_id={}",
        project_id
    );

    // 获取全局 DockerManager 实例
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| anyhow::anyhow!("获取全局 DockerManager 失败: {}", e))?;

    // 获取容器信息
    let container_info = docker_manager
        .get_container_info(project_id)
        .ok_or_else(|| anyhow::anyhow!("未找到容器信息: project_id={}", project_id))?;

    // 获取动态网络名称
    let container_manager = crate::service::container_manager::ContainerManager;
    let network_name = container_manager
        .get_dynamic_network_name(&docker_manager)
        .await;

    // 获取容器 IP
    let container_ip = crate::proxy_agent::docker_container_agent::get_container_ip(
        &docker_manager,
        &container_info.container_id,
        &network_name,
    )
    .await
    .map_err(|e| anyhow::anyhow!("获取容器 IP 失败: {}", e))?;

    // 移除 http:// 前缀（如果有）并提取主机名
    let host = container_ip
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or(&container_ip);

    let grpc_addr = format!("{}:{}", host, grpc_port);

    info!("✅ [CONTAINER] 获取容器 gRPC 地址: {}", grpc_addr);
    Ok(grpc_addr)
}

use std::{
    path::{Component, PathBuf},
    sync::{Arc, LazyLock},
};

use agent_client_protocol::{ContentBlock, PromptRequest, SessionId, TextContent}; // bring trait into scope for session_notification

use chrono::Utc;
use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::{
    model::{AgentStatus, ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::agent_service::AcpAgentService,
    utils::{ContentBuilder, PromptBuilder},
};

use anyhow::Result;

/// 使用 OnceLock 和 DashMap 管理 ProjectAndAgentInfo
pub static PROJECT_AND_AGENT_INFO_MAP: LazyLock<DashMap<String, ProjectAndAgentInfo>> =
    LazyLock::new(DashMap::new);

/// 检查模型配置是否发生变化
fn check_model_config_changed(
    existing_config: &Option<ModelProviderConfig>,
    new_config: &Option<ModelProviderConfig>,
) -> bool {
    match (existing_config, new_config) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(existing), Some(new)) => {
            // 比较模型配置的id字段，如果不同则认为模型配置发生了变化
            existing.id != new.id
        }
    }
}

/// 创建新的Agent服务
async fn create_new_agent_service(
    request: LocalSetAgentRequest,
    chat_prompt: ChatPrompt,
    project_id: String,
) {
    info!("开始创建新的Agent服务，项目ID: {}", project_id);

    // 根据 chat_prompt.agent_type 自动判断使用 codex 还是 claude code
    let start_agent_result = chat_prompt
        .agent_type
        .start_agent_service(chat_prompt.clone(), request.model_provider.clone())
        .await;

    // 创建 agent 服务
    match start_agent_result {
        Ok(conn_info) => {
            // 使用现有的AgentStopHandle作为dyn AgentLifecycle
            let stop_handle = conn_info
                .stop_handle
                .as_ref()
                .map(|guard| guard.clone() as Arc<dyn shared_types::AgentLifecycle>);

            let project_and_agent_info = ProjectAndAgentInfo {
                project_id: project_id.clone(),
                session_id: conn_info.session_id.clone(),
                prompt_tx: conn_info.prompt_tx.clone(),
                cancel_tx: conn_info.cancel_tx.clone(),
                model_provider: request.model_provider.clone(),
                request_id: request.chat_prompt.request_id.clone(),
                status: AgentStatus::Idle,
                last_activity: Utc::now(),
                created_at: Utc::now(),
                stop_handle,
            };

            // 记录项目project_id和 agent 服务信息的映射,一个project_id对应一个 agent 服务,方便复用agent 服务
            PROJECT_AND_AGENT_INFO_MAP.insert(project_id.clone(), project_and_agent_info.clone());

            let session_id_str = conn_info.session_id.to_string();

            // 建立 project_id -> session_id 映射，确保 cleanup 任务能正确识别活跃 session
            let cleared_old =
                crate::service::ensure_project_session(&project_id, &session_id_str).await;
            if cleared_old > 0 {
                info!(
                    "🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
                    project_id, cleared_old
                );
            } else {
                info!(
                    "🔗 Project session 映射已同步: project_id={}, session_id={}",
                    project_id, session_id_str
                );
            }

            let response =
                match build_prompt_to_acp_agent(chat_prompt.clone(), conn_info.session_id.clone())
                    .await
                {
                    Ok(prompt_request) => {
                        if let Err(e) = conn_info.prompt_tx.send(prompt_request) {
                            error!("发送prompt请求失败: {:?}", e);
                            ChatPromptResponse {
                                project_id: project_id.clone(),
                                session_id: conn_info.session_id.to_string(),
                                error: Some(format!("发送prompt请求失败: {:?}", e)),
                                request_id: chat_prompt.request_id.clone(),
                                service_type: chat_prompt.service_type.clone(),
                            }
                        } else {
                            info!("Prompt 请求已发送，项目ID: {}", project_id);
                            ChatPromptResponse {
                                project_id: project_id.clone(),
                                session_id: conn_info.session_id.to_string(),
                                error: None,
                                request_id: chat_prompt.request_id.clone(),
                                service_type: chat_prompt.service_type.clone(),
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "❌ 构建prompt请求失败，项目ID: {}，错误详情: {:?}",
                            project_id, e
                        );
                        ChatPromptResponse {
                            project_id: project_id.clone(),
                            session_id: conn_info.session_id.to_string(),
                            error: Some(format!("构建prompt请求失败: {:?}", e)),
                            request_id: chat_prompt.request_id.clone(),
                            service_type: chat_prompt.service_type.clone(),
                        }
                    }
                };

            // 发送回执消息
            if let Err(e) = request.chat_prompt_tx.send(response) {
                error!("发送chat prompt响应失败: {:?}", e);
            }
        }
        Err(e) => {
            error!("启动ACP Agent服务失败，项目ID: {}, 错误: {}", project_id, e);

            // 发送失败回执给前端
            let error_response = ChatPromptResponse {
                project_id: project_id.clone(),
                session_id: "".to_string(), // 启动失败时没有 session_id
                error: Some(format!("启动ACP Agent服务失败: {}", e)),
                request_id: chat_prompt.request_id.clone(),
                service_type: chat_prompt.service_type.clone(),
            };

            if let Err(send_err) = request.chat_prompt_tx.send(error_response) {
                error!(
                    "发送启动失败回执失败，项目ID: {}, 错误: {:?}",
                    project_id, send_err
                );
            }
        }
    }
}

/// 在 LocalSet 中运行的实际 Agent 请求
#[derive(Debug)]
pub struct LocalSetAgentRequest {
    /// 用户端发送的 prompt 请求
    chat_prompt: ChatPrompt,
    /// 发送 agent 通知执行prompt 完毕的回执消息
    chat_prompt_tx: oneshot::Sender<ChatPromptResponse>,
    /// 模型提供商配置
    model_provider: Option<ModelProviderConfig>,
}

impl LocalSetAgentRequest {
    pub fn new(
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> (Self, oneshot::Receiver<ChatPromptResponse>) {
        let (chat_prompt_tx, chat_prompt_rx) = oneshot::channel();

        (
            Self {
                chat_prompt,
                chat_prompt_tx,
                model_provider,
            },
            chat_prompt_rx,
        )
    }
}

/// AgentSideConnection , ClientSideConnection 没实现 Send trait ,需要在 LocalSet 中运行,另外 agent服务数量是动态的,和 project_id 是一一对应的,一个 project_id 对应一个 agent服务

/// Agent worker 任务，在本地线程中运行 Agent
pub async fn agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<LocalSetAgentRequest>,
) -> Result<()> {
    info!("🚀 agent_worker 启动，开始监听请求...");
    while let Some(request) = request_rx.recv().await {
        info!(
            "📨 agent_worker 接收到新请求，project_id: {}",
            request.chat_prompt.project_id
        );
        let mut chat_prompt = request.chat_prompt.clone();

        let original_path = chat_prompt.project_path;
        // 规范化路径：
        // - 如果是相对路径，先与当前目录拼接
        // - 去除路径中的 "./"（CurDir 组件），不依赖文件系统
        let joined_path = if original_path.is_absolute() {
            original_path
        } else {
            std::env::current_dir().unwrap().join(original_path)
        };
        let project_path: PathBuf = joined_path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        // 将规范化后的路径写回，确保后续使用统一
        chat_prompt.project_path = project_path.clone();

        // 创建项目目录
        if !project_path.exists() {
            info!(
                "Project path does not exist,project_id={}",
                request.chat_prompt.project_id
            );
            //自动创建目录
            if let Err(e) = tokio::fs::create_dir_all(&project_path).await {
                error!("Failed to create project directory: {:?}", e);
                continue;
            }
        }

        info!(
            "🔍 处理完路径，准备查找Agent，project_id: {}",
            request.chat_prompt.project_id
        );

        // 检查 project_id 有对应的agent 服务,没有则创建
        let project_id = request.chat_prompt.project_id.clone();

        // 先检查是否存在Agent并提取必要信息，然后立即释放锁
        let agent_exists = PROJECT_AND_AGENT_INFO_MAP.contains_key(&project_id);

        if agent_exists {
            info!("✅ 找到现有Agent，project_id: {}", project_id);
        } else {
            info!(
                "❌ 未找到现有Agent，将创建新Agent，project_id: {}",
                project_id
            );
        }

        // 检查是否需要因模型配置变化而重启Agent服务
        let need_restart_agent = if agent_exists {
            let agent_info = PROJECT_AND_AGENT_INFO_MAP.get(&project_id);
            if let Some(ref info) = agent_info {
                check_model_config_changed(&info.model_provider, &request.model_provider)
            } else {
                false
            }
            // agent_info 在这里自动释放，读锁被释放
        } else {
            false
        };

        if agent_exists && need_restart_agent {
            // 模型配置发生变化，需要重启Agent服务
            info!("检测到模型配置变化，重启Agent服务，项目ID: {}", project_id);

            // ⚠️ 此时不持有任何锁，可以安全地获取写锁并 remove
            PROJECT_AND_AGENT_INFO_MAP.remove(&project_id);

            // 同步清理 SESSION_REQUEST_CONTEXT 中的 request_id
            crate::proxy_agent::SESSION_REQUEST_CONTEXT.remove(&project_id);
            debug!(
                "🧼 [acp_agent] 已清理 SESSION_REQUEST_CONTEXT 中的 project_id={}",
                project_id
            );

            // 创建新的Agent服务
            create_new_agent_service(request, chat_prompt, project_id).await;
        } else if agent_exists {
            // 模型配置未变化，复用现有Agent服务
            info!("复用现有Agent服务，项目ID: {}", project_id);

            // 重新获取agent_info（短暂持有读锁）
            if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(&project_id) {
                let session_id = agent_info.session_id.clone();
                let prompt_tx = agent_info.prompt_tx.clone();
                // 读锁在此处自动释放

                debug!("复用Agent - session_id: {}, prompt_tx可用", session_id.0);

                // 注意：不在这里更新 request_id，因为 get_mut 会需要写锁，
                // 可能与正在执行的 Prompt 处理任务产生锁冲突。
                // 直接将 request_id 包含在 ChatPrompt 中，在构建 PromptRequest 时使用。
                debug!("使用请求中的request_id: {:?}", chat_prompt.request_id);

                debug!("开始构建Prompt请求，项目ID: {}", project_id);
                match build_prompt_to_acp_agent(chat_prompt, session_id.clone()).await {
                    Ok(prompt_request) => {
                        info!(
                            "Prompt请求构建成功，准备发送到channel，项目ID: {}",
                            project_id
                        );
                        if let Err(e) = prompt_tx.send(prompt_request) {
                            error!("❌ 发送Prompt请求到channel失败: {:?}", e);
                        } else {
                            info!("✅ Prompt请求已成功发送到channel，项目ID: {}", project_id);
                        }
                    }
                    Err(e) => {
                        error!("❌ 构建Prompt请求失败: {}", e);
                    }
                }

                // 发送回执消息
                debug!("准备发送回执消息，项目ID: {}", project_id);
                if let Err(e) = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: session_id.to_string(),
                    error: None,
                    request_id: request.chat_prompt.request_id.clone(),
                    service_type: request.chat_prompt.service_type.clone(),
                }) {
                    error!("发送回执消息失败: {:?}", e);
                } else {
                    info!("✅ 回执消息发送成功，项目ID: {}", project_id);
                }
            }
        } else {
            // 创建新的Agent服务
            create_new_agent_service(request, chat_prompt, project_id).await;
        }
    }
    debug!("Agent worker finished");
    Ok(())
}

/// 构建 Prompt 请求
pub async fn build_prompt_to_acp_agent(
    prompt: ChatPrompt,
    session_id: SessionId,
) -> Result<PromptRequest> {
    // 构建最终提示词（包含系统提示词、用户输入和数据源信息）
    let final_prompt = if prompt.data_source_attachments.is_empty() {
        PromptBuilder::new().build(&prompt.prompt)
    } else {
        PromptBuilder::new()
            .build_with_data_sources(&prompt.prompt, &prompt.data_source_attachments)
    };

    // 创建文本内容块
    let text_block = ContentBlock::Text(TextContent::new(final_prompt));

    // 创建内容块列表，以文本开始
    let mut content_blocks = vec![text_block];

    // 如果有附件，转换为内容块
    if !prompt.attachments.is_empty() {
        let attachment_blocks = ContentBuilder::attachments_to_content_blocks(
            &prompt.attachments,
            &prompt.project_path,
        )
        .await?;

        content_blocks.extend(attachment_blocks);
    }

    // 将 request_id 放入 meta 字段，以便 channel_utils 可以提取并更新到 MAP
    let prompt_request = if let Some(request_id) = prompt.request_id {
        debug!(
            "🔧 [build_prompt] 将 request_id={} 放入 PromptRequest.meta",
            request_id
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "request_id".to_string(),
            serde_json::Value::String(request_id),
        );
        PromptRequest::new(session_id, content_blocks).meta(meta)
    } else {
        debug!("⚠️ [build_prompt] prompt.request_id 为空，不设置 meta");
        PromptRequest::new(session_id, content_blocks)
    };

    Ok(prompt_request)
}

use agent_client_protocol::{
    Agent, ClientCapabilities, ClientSideConnection, ContentBlock, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, SessionId, TextContent, V1 as VERSION,
};

use shared_types::ModelProviderConfig;
use std::{process::Stdio, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
    AgentType, CancelNotificationRequest,
    model::ChatPrompt,
    proxy_agent::{AcpAgentClient, AcpConnectionInfo, agent_stop_handle::AgentLifecycleGuard},
    utils::create_default_mcp_servers,
};
use anyhow::{Context, Result};
use tokio::task::LocalSet;

/// 启动一个长驻的 Codex ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 使用子进程方式启动 codex-acp-agent（fork 版本支持 -c 参数）
pub async fn start_codex_acp_agent_service(
    chat_prompt: ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
) -> Result<AcpConnectionInfo> {
    let project_path = chat_prompt.project_path;

    // 获取 codex-acp-agent 命令路径
    let command_path = "codex-acp-agent";
    
    info!("Codex ACP Agent 命令: {}", command_path);
    

    // 用户发送 CancelNotification 消息的通道
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelNotificationRequest>();

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    // 创建 CancellationToken 用于控制子进程生命周期
    let cancel_token = CancellationToken::new();

    // 克隆用于闭包
    let prompt_tx_for_closure = prompt_tx.clone();
    let project_path_for_closure = project_path.clone();
    let project_id_for_child = chat_prompt.project_id.clone();
    let cancel_token_for_closure = cancel_token.clone();

    info!(
        "项目工作目录: {}",
        &project_path_for_closure.to_string_lossy()
    );

    // 准备环境变量和 CLI 参数
    let (_, merged_envs) = AgentType::codex_model_provider(model_provider.clone()).await?;
    info!("🔑 为 Codex ACP 子进程准备了 {} 个环境变量", merged_envs.len());
    for (key, value) in &merged_envs {
        // 只显示前6位和后4位，中间用*隐藏
        let masked_value = if value.len() > 10 {
            format!("{}...{}", &value[..6], &value[value.len()-4..])
        } else {
            "***".to_string()
        };
        info!("  - {}={}", key, masked_value);
    }
    info!("✨ 使用 openai-api-key 认证方法，codex-acp-agent 将自动进行认证");
    
    // 构建 CLI 配置覆盖参数（-c key=value 格式）
    let mut cli_args = Vec::<String>::new();
    
    if let Some(ref provider) = model_provider {
        // 1. 设置模型
        if !provider.default_model.is_empty() {
            cli_args.push("-c".to_string());
            cli_args.push(format!("model={}", provider.default_model));
        }
        
        // 2. 设置模型提供商
        // 固定使用 "custom"，与下面的 model_providers.custom.* 配置保持一致
        cli_args.push("-c".to_string());
        cli_args.push("model_provider=custom".to_string());
        
        // 3. 配置模型提供商的详细信息
        // 使用固定的 provider_name，避免中文或特殊字符问题
        // 由于每个子进程配置独立，不会有命名冲突
        let provider_name = "custom";
        
        // base_url
        if !provider.base_url.is_empty() {
            cli_args.push("-c".to_string());
            cli_args.push(format!(
                "model_providers.{}.base_url={}",
                provider_name, provider.base_url
            ));
        }
        
        // env_key (API key 环境变量名，使用 CODEX_API_KEY)
        cli_args.push("-c".to_string());
        cli_args.push(format!(
            "model_providers.{}.env_key=CODEX_API_KEY",
            provider_name
        ));
        
        // requires_openai_auth
        if provider.requires_openai_auth {
            cli_args.push("-c".to_string());
            cli_args.push(format!(
                "model_providers.{}.requires_openai_auth=true",
                provider_name
            ));
        }
        
        // wire_api (chat/completions)
        cli_args.push("-c".to_string());
        cli_args.push(format!(
            "model_providers.{}.wire_api=chat",
            provider_name
        ));
        
        // name
        cli_args.push("-c".to_string());
        cli_args.push(format!(
            "model_providers.{}.name={}",
            provider_name, provider_name
        ));
        // 配置 认证方式
        cli_args.push("-c".to_string());
        cli_args.push("preferred_auth_method=codex-api-key".to_string());
        
        info!("✨ Codex ACP CLI 配置覆盖参数: {:?}", cli_args);
    }
    
    // 启动子进程，传递 CLI 配置覆盖参数
    let mut child = tokio::process::Command::new(command_path)
        .args(&cli_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .current_dir(&project_path_for_closure)
        .envs(merged_envs)
        .spawn()
        .context("无法启动 codex-acp 子进程")?;

    let child_pid = child.id().unwrap_or(0);
    info!("Codex ACP 子进程已启动，PID: {}", child_pid);

    // 获取 stdio 句柄
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stderr"))?;

    // 创建兼容的流
    let outgoing = stdin.compat_write();
    let incoming = stdout.compat();

    // 创建客户端
    let client = AcpAgentClient;

    // 创建连接
    let (client_conn, handle_io) = ClientSideConnection::new(client, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });

    // 启动 I/O 处理任务
    tokio::task::spawn_local(handle_io);

    let client_conn = Arc::new(client_conn);

    // 启动后台任务来管理 ACP 连接
    let _task_handle = tokio::task::spawn_local(async move {
        // 创建 LocalSet 来运行非 Send 的 ACP 连接
        let local_set = LocalSet::new();

        let result = local_set
            .run_until(async {
                let client_conn = client_conn.clone();
                // 初始化连接
                debug!("初始化 ACP 连接[initialize]");
                let init_result = client_conn
                    .initialize(InitializeRequest {
                        protocol_version: VERSION,
                        client_capabilities: ClientCapabilities {
                            fs: agent_client_protocol::FileSystemCapability {
                                read_text_file: false,
                                write_text_file: false,
                                meta: None,
                            },
                            terminal: false,
                            meta: None,
                        },
                        meta: None,
                    })
                    .await;

                match init_result {
                    Ok(_) => {
                        info!("ACP 连接初始化成功");
                    }
                    Err(e) => {
                        error!("ACP 连接初始化失败: {:?}", e);
                        return Err(anyhow::anyhow!(
                            "Failed to initialize ACP connection: {:?}",
                            e
                        ));
                    }
                }

                // 创建 MCP 服务器配置
                let mcp_servers = create_default_mcp_servers(None);

                if !mcp_servers.is_empty() {
                    info!(
                        "🔧 配置了 {} 个 MCP 服务器: {}",
                        mcp_servers.len(),
                        mcp_servers
                            .iter()
                            .map(|s| match s {
                                agent_client_protocol::McpServer::Stdio { name, .. } =>
                                    name.clone(),
                                _ => "unknown".to_string(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else {
                    info!("📝 未配置 MCP 服务器");
                }

                // 创建会话
                let session_id = match chat_prompt.session_id {
                    Some(session_id) => {
                        debug!("尝试加载 ACP 会话[load_session]");
                        let given_session_id = SessionId(session_id.into());
                        match client_conn
                            .load_session(LoadSessionRequest {
                                session_id: given_session_id.clone(),
                                mcp_servers: mcp_servers.clone(),
                                cwd: project_path_for_closure.clone(),
                                meta: None,
                            })
                            .await
                        {
                            Ok(resp) => {
                                debug!("ACP 会话加载成功[load_session],{:?}", resp);
                                given_session_id
                            }
                            Err(e) => {
                                warn!("load_session 失败，回退创建新会话[new_session]: {:?}", e);
                                let resp = client_conn
                                    .new_session(NewSessionRequest {
                                        mcp_servers: mcp_servers.clone(),
                                        cwd: project_path_for_closure.clone(),
                                        meta: None,
                                    })
                                    .await?;
                                debug!("ACP 会话创建成功[new_session],{:?}", resp);
                                resp.session_id
                            }
                        }
                    }
                    None => {
                        debug!("创建 ACP 会话[new_session]");
                        let resp = client_conn
                            .new_session(NewSessionRequest {
                                mcp_servers: mcp_servers.clone(),
                                cwd: project_path_for_closure.clone(),
                                meta: None,
                            })
                            .await?;
                        debug!("ACP 会话创建成功[new_session],{:?}", resp);
                        resp.session_id
                    }
                };

                // 发送会话 ID 到主线程
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("无法发送会话 ID：接收方已关闭");
                    return Err(anyhow::anyhow!("无法发送会话 ID"));
                }

                // 使用共享的通道处理逻辑
                super::channel_utils::spawn_cancel_handler_for_agent(
                    client_conn.clone(),
                    cancel_rx,
                    &chat_prompt.project_id,
                );
                super::channel_utils::spawn_prompt_handler_for_agent(
                    client_conn.clone(),
                    prompt_rx,
                    session_id.clone(),
                    &chat_prompt.project_id,
                );

                // 等待取消信号
                cancel_token_for_closure.cancelled().await;
                info!("Codex ACP Agent 收到取消信号，将清理资源并退出");
                Ok(())
            })
            .await;

        if let Err(e) = result {
            error!("Codex ACP Agent 后台任务失败: {:?}", e);
            error!("可能原因：1) base_url 配置错误 2) API key 无效 3) 模型名称不支持");
            // 通知主线程任务失败
            let error_block = ContentBlock::Text(TextContent {
                text: format!("Codex ACP Agent 启动失败: {:?}", e),
                annotations: None,
                meta: None,
            });
            let _ = prompt_tx_for_closure.send(PromptRequest {
                session_id: SessionId("error".into()),
                prompt: vec![error_block],
                meta: None,
            });
        }
    });

    // 等待会话 ID 并立即返回
    let session_id = session_id_rx.await.map_err(|e| {
        error!("等待会话 ID 失败: {}", e);
        anyhow::anyhow!("等待会话 ID 失败: {}", e)
    })?;

    info!(
        "Codex ACP Agent 服务启动完成，会话 ID: {}",
        session_id.0
    );

    // 创建stderr任务来处理子进程的stderr输出
    let cancel_token_for_stderr = cancel_token.clone();
    let stderr_task = tokio::task::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut stderr_reader = tokio::io::BufReader::new(stderr);
        let mut stderr_buffer = String::new();

        loop {
            // 检查取消令牌
            if cancel_token_for_stderr.is_cancelled() {
                info!("Codex ACP Agent stderr 任务收到取消信号，退出读取");
                break;
            }

            match stderr_reader.read_line(&mut stderr_buffer).await {
                Ok(0) => {
                    info!("Codex ACP Agent stderr 流已关闭");
                    break;
                }
                Ok(bytes_read) => {
                    let line = &stderr_buffer[..bytes_read];
                    if !line.trim().is_empty() {
                        warn!("Codex ACP Agent stderr: {}", line.trim());
                    }
                    stderr_buffer.clear();
                }
                Err(e) => {
                    error!("读取 Codex ACP Agent stderr 失败: {}", e);
                    break;
                }
            }
        }
    });

    // 创建生命周期守卫
    let lifecycle_guard = AgentLifecycleGuard::new_codex(
        project_id_for_child.clone(),
        session_id.clone(),
        child,
        stderr_task,
        cancel_token.clone(),
    );

    Ok(AcpConnectionInfo {
        session_id,
        prompt_tx,
        cancel_tx,
        stop_handle: Some(Arc::new(lifecycle_guard)),
    })
}

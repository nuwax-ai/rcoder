use agent_client_protocol::{
    self as acp, Agent, ClientCapabilities, ClientSideConnection, ContentBlock, InitializeRequest,
    NewSessionRequest, PromptRequest, SessionId, TextContent, V1 as VERSION,
};

use claude::ClaudeCodeAcpManager;
use std::process::Stdio;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::model::ChatPrompt;
use anyhow::{Context, Result};
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Claude Code ACP 客户端实现
///
/// 实现了 acp::Client trait，用于处理来自代理的请求和通知
pub struct ClaudeCodeAcpClient {
    session_notifications: tokio::sync::mpsc::UnboundedSender<acp::SessionNotification>,
}

impl ClaudeCodeAcpClient {
    /// 创建新的客户端实例
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        Self {
            session_notifications: tx,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for ClaudeCodeAcpClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        // 转发会话通知到通道
        let _ = self.session_notifications.send(args);
        Ok(())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Err(acp::Error::method_not_found())
    }
}

/// 启动一个长驻的 Claude Code ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 使用 claude-code-acp 作为代理服务，通过子进程方式启动
pub async fn start_claude_code_acp_agent_service(
    chat_prompt: ChatPrompt,
) -> Result<(SessionId, mpsc::UnboundedSender<PromptRequest>)> {
    let project_path = chat_prompt.project_path;

    // 创建 Claude Code ACP 管理器（使用默认配置）
    let manager = ClaudeCodeAcpManager::default();
    info!("Claude Code ACP 管理器创建完成");

    // 检查并安装 claude-code-acp
    if !manager.is_available().await {
        info!("Claude Code ACP 未安装，正在安装...");
        manager.install_or_update().await.map_err(|e| {
            error!("Failed to install claude-code-acp: {}", e);
            anyhow::anyhow!("Failed to install claude-code-acp: {}", e)
        })?;
        info!("Claude Code ACP 安装完成");
    }

    // 获取启动命令
    let command = manager.get_command().await.map_err(|e| {
        error!("Failed to get claude-code-acp command: {}", e);
        anyhow::anyhow!("Failed to get claude-code-acp command: {}", e)
    })?;
    info!(
        "Claude Code ACP 命令: {:?} {:?}",
        command.path, command.args
    );

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    // 克隆用于闭包
    let prompt_tx_for_closure = prompt_tx.clone();
    let project_path_for_closure = project_path.clone();

    // 启动后台任务来管理子进程和 ACP 连接
    tokio::task::spawn_local(async move {
        // 创建 LocalSet 来运行非 Send 的 ACP 连接
        let local_set = LocalSet::new();

        let result = local_set
            .run_until(async {
                // 启动子进程
                let mut child = tokio::process::Command::new(&command.path)
                    .args(&command.args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .current_dir(&project_path_for_closure)
                    .envs(command.env.unwrap_or_default())
                    .spawn()
                    .context("无法启动 claude-code-acp 子进程")?;

                info!(
                    "Claude Code ACP 子进程已启动，PID: {}",
                    child.id().unwrap_or(0)
                );

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

                // 启动 stderr 读取任务
                tokio::task::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut stderr_reader = tokio::io::BufReader::new(stderr);
                    let mut stderr_buffer = String::new();

                    loop {
                        match stderr_reader.read_line(&mut stderr_buffer).await {
                            Ok(0) => {
                                info!("Claude Code ACP stderr 流已关闭");
                                break;
                            }
                            Ok(bytes_read) => {
                                let line = &stderr_buffer[..bytes_read];
                                if !line.trim().is_empty() {
                                    warn!("Claude Code ACP stderr: {}", line.trim());
                                }
                                stderr_buffer.clear();
                            }
                            Err(e) => {
                                error!("读取 Claude Code ACP stderr 失败: {}", e);
                                break;
                            }
                        }
                    }
                });

                // 创建兼容的流
                let outgoing = stdin.compat_write();
                let incoming = stdout.compat();

                // 创建客户端
                let client = ClaudeCodeAcpClient::new();

                // 创建连接
                let (client_conn, handle_io) =
                    ClientSideConnection::new(client, outgoing, incoming, |fut| {
                        tokio::task::spawn_local(fut);
                    });

                // 启动 I/O 处理任务
                tokio::task::spawn_local(handle_io);

                // 初始化连接
                debug!("初始化 ACP 连接[initialize]");
                info!("正在发送 ACP 初始化请求...");
                let init_result = client_conn
                    .initialize(InitializeRequest {
                        protocol_version: VERSION,
                        client_capabilities: ClientCapabilities::default(),
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

                // 创建会话
                debug!("创建 ACP 会话[new_session]");
                let session_result = tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    client_conn.new_session(NewSessionRequest {
                        mcp_servers: Vec::new(),
                        cwd: project_path_for_closure.clone(),
                        meta: None,
                    }),
                )
                .await;

                let session_resp = match session_result {
                    Ok(Ok(resp)) => {
                        debug!("ACP 会话创建成功");
                        resp
                    }
                    Ok(Err(e)) => {
                        error!("ACP 会话创建失败: {:?}", e);
                        return Err(anyhow::anyhow!("Failed to create ACP session: {:?}", e));
                    }
                    Err(_) => {
                        error!("ACP 会话创建超时");
                        return Err(anyhow::anyhow!("ACP session creation timeout"));
                    }
                };

                info!("ACP 会话已创建，ID: {}", session_resp.session_id.0);

                // 发送会话 ID 到主线程
                let session_id = session_resp.session_id.clone();
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("无法发送会话 ID：接收方已关闭");
                    return Err(anyhow::anyhow!("无法发送会话 ID"));
                }

                // 长驻循环：接收外部 prompt 并转发到 ACP
                tokio::task::spawn_local({
                    let session_id_for_prompt = session_id.clone();
                    async move {
                        while let Some(mut req) = prompt_rx.recv().await {
                            if req.session_id.0.is_empty() {
                                req.session_id = session_id_for_prompt.clone();
                            }
                            match client_conn.prompt(req).await {
                                Ok(_) => {
                                    debug!("Prompt 发送成功");
                                }
                                Err(e) => {
                                    error!("发送 Prompt 失败: {:?}", e);
                                }
                            }
                        }
                        debug!("Prompt 接收通道已关闭");
                    }
                });

                // 等待子进程结束（如果进程意外退出）
                let child_exit = child.wait();
                tokio::pin!(child_exit);

                // 创建一个未来来保持连接活跃
                let keep_alive = async {
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                        debug!("Claude Code ACP Agent 服务保持活跃");
                    }
                };

                tokio::select! {
                    result = child_exit => {
                        match result {
                            Ok(exit_status) => {
                                error!("Claude Code ACP 子进程退出，状态: {}", exit_status);
                            }
                            Err(e) => {
                                error!("Claude Code ACP 子进程等待失败: {}", e);
                            }
                        }
                        Err(anyhow::anyhow!("Claude Code ACP 子进程意外退出"))
                    }
                    _ = keep_alive => {
                        Ok(session_id.clone())
                    }
                }
            })
            .await;

        if let Err(e) = result {
            error!("Claude Code ACP Agent 后台任务失败: {}", e);
            // 通知主线程任务失败 - 发送一个错误提示作为信号
            let error_block = ContentBlock::Text(TextContent {
                text: format!("Claude Code ACP Agent 启动失败: {}", e),
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
        "Claude Code ACP Agent 服务启动完成，会话 ID: {}",
        session_id.0
    );
    Ok((session_id, prompt_tx))
}

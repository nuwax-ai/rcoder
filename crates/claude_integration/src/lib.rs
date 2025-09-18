use agent_client_protocol::{
    self as acp,
    AgentSideConnection,
    Client,
    ClientCapabilities,
    FileSystemCapability,
    SessionId,
    Agent,
    ClientResponse,
    AgentRequest,
    RequestPermissionRequest,
    RequestPermissionResponse,
    RequestPermissionOutcome,
    WriteTextFileRequest,
    WriteTextFileResponse,
    ReadTextFileRequest,
    ReadTextFileResponse,
    SessionNotification,
    CreateTerminalRequest,
    CreateTerminalResponse,
    TerminalOutputRequest,
    TerminalOutputResponse,
    ReleaseTerminalRequest,
    ReleaseTerminalResponse,
    WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
    KillTerminalCommandRequest,
    KillTerminalCommandResponse,
    TerminalId,
    TerminalExitStatus,
    PermissionOption,
    PermissionOptionId,
    PermissionOptionKind,
    ExtRequest,
    ExtResponse,
    ExtNotification,
    ContentBlock,
    TextContent,
    Resource,
    ResourceContents,
    TextResourceContents,
    InitializeRequest,
    InitializeResponse,
    AgentCapabilities,
    AuthMethod,
    AuthMethodId,
    NewSessionRequest,
    NewSessionResponse,
    PromptRequest,
    PromptResponse,
    StopReason,
    McpServer,
    SessionModeState,
    SessionMode,
    SessionModeId,
    CancelNotification,
    Error,
    EnvVariable,
    HttpHeader,
    SessionUpdate,
    AvailableCommand,
    AvailableCommandInput,
};
use anyhow::Result;
use serde_json::json;
use shared_types::{Project, CreateProjectRequest, FileChange, FileChangeType};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    process::Stdio,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Command,
    sync::RwLock,
};
use tracing::{debug, info, error, warn};
use uuid::Uuid;

pub mod project_builder;
pub mod template_manager;

pub use project_builder::ProjectBuilder;
pub use template_manager::TemplateManager;

/// ACP Client implementation for handling file operations and permissions
pub struct AcpClientImpl {
    /// Current working directory for file operations
    working_dir: PathBuf,
    /// Project management functionality
    project_manager: Arc<ProjectManager>,
    /// Active sessions
    sessions: Arc<RwLock<HashMap<Uuid, SessionInfo>>>,
    /// Permission cache for remembering user decisions
    permission_cache: Arc<RwLock<HashMap<String, bool>>>,
}

/// Session information
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: Uuid,
    pub acp_session_id: SessionId,
    pub project_path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Main Claude Code integration manager
pub struct ClaudeCodeManager {
    /// ACP client implementation
    acp_client: Arc<AcpClientImpl>,
    /// Project builder for creating project structures
    project_builder: ProjectBuilder,
    /// Template manager for project templates
    template_manager: TemplateManager,
    /// Active ACP connections
    connections: Arc<RwLock<HashMap<String, Arc<AgentSideConnection>>>>,
}

impl ClaudeCodeManager {
    pub async fn new(working_dir: PathBuf) -> Result<Self> {
        let project_manager = Arc::new(ProjectManager::new("sqlite://projects.db").await?);
        let acp_client = Arc::new(AcpClientImpl::new(working_dir.clone(), project_manager.clone()));
        let project_builder = ProjectBuilder::new();
        let template_manager = TemplateManager::new();

        Ok(Self {
            acp_client,
            project_builder,
            template_manager,
            connections: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Initialize the ACP connection to Claude Code
    pub async fn initialize(&mut self) -> Result<()> {
        info!("Initializing Claude Code manager with ACP protocol");

        // Create Claude Code process
        let mut child = Command::new("claude")
            .args(["--acp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to start Claude Code: {}", e))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Create ACP connection
        let (connection, io_task) = AgentSideConnection::new(
            self.acp_client.clone(),
            stdin,
            stdout,
            |future| {
                tokio::spawn(future);
            }
        );

        // Start the IO task
        tokio::spawn(io_task);

        // Store the connection
        let mut connections = self.connections.write().await;
        connections.insert("default".to_string(), Arc::new(connection));

        info!("Claude Code manager initialized successfully");
        Ok(())
    }

    /// Create a new project and start an ACP session
    pub async fn create_project(
        &self,
        project_name: &str,
        description: Option<&str>,
        template: Option<&str>,
        base_path: Option<&PathBuf>,
    ) -> Result<Uuid> {
        info!("Creating project: {}", project_name);

        // Create project directory structure
        let project_path = self.project_builder.create_project_structure(
            project_name,
            description,
            template,
            base_path,
        ).await?;

        let project_id = Uuid::new_v4();

        // Create project record
        let create_request = CreateProjectRequest {
            name: project_name.to_string(),
            description: description.map(|s| s.to_string()),
            template: template.map(|s| s.to_string()),
            path: Some(project_path.clone()),
        };

        let project = self.acp_client.project_manager.create_project(create_request).await?;

        // Create ACP session for this project
        let connection = self.connections.read().await
            .get("default")
            .ok_or_else(|| anyhow::anyhow!("No ACP connection available"))?
            .clone();

        // Create new session request
        let session_request = NewSessionRequest {
            cwd: project_path.clone(),
            mcp_servers: vec![],
            meta: None,
        };

        let session_response = connection.new_session(session_request).await?;
        let session_id = Uuid::new_v4();

        // Store session info
        let session_info = SessionInfo {
            session_id,
            acp_session_id: session_response.session_id,
            project_path: project_path.clone(),
            created_at: chrono::Utc::now(),
        };

        let mut sessions = self.acp_client.sessions.write().await;
        sessions.insert(session_id, session_info);

        // Initialize the project with Claude Code
        let init_prompt = if let Some(template) = template {
            format!(
                "Create a new {} project named '{}'{}. {}",
                template,
                project_name,
                description.map_or("".to_string(), |d| format!(" with description: {}", d)),
                "Set up the basic project structure, configuration files, and build system."
            )
        } else {
            format!(
                "Create a new project named '{}'. Set up the basic project structure, configuration files, and build system.",
                project_name,
                description.map_or("".to_string(), |d| format!(" with description: {}", d))
            )
        };

        let prompt_request = PromptRequest {
            session_id: session_response.session_id,
            prompt: vec
![ContentBlock::Text(TextContent {
                text: init_prompt,
                annotations: None,
                meta: None,
            })],
            meta: None,
        };

        let response = connection.prompt(prompt_request).await?;

        debug!("Project creation response: {:?}", response);

        Ok(project_id)
    }

    /// Process a prompt for an existing project
    pub async fn process_prompt(
        &self,
        project_id: Uuid,
        prompt: &str,
        context_files: Option<Vec<PathBuf>>,
    ) -> Result<PromptResponse> {
        info!("Processing prompt for project {}: {}", project_id, prompt);

        // Get session info
        let sessions = self.acp_client.sessions.read().await;
        let session_info = sessions.get(&project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found for project: {}", project_id))?;

        // Get connection
        let connection = self.connections.read().await
            .get("default")
            .ok_or_else(|| anyhow::anyhow!("No ACP connection available"))?
            .clone();

        // Build prompt with context
        let mut content_blocks = vec
![ContentBlock::Text(TextContent {
            text: prompt.to_string(),
            annotations: None,
            meta: None,
        })];

        // Add context files if provided
        if let Some(files) = context_files {
            for file_path in files {
                if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                    content_blocks.push(ContentBlock::Resource(Resource {
                        uri: format!("file://{}", file_path.display()),
                        contents: Some(ResourceContents::Text(TextResourceContents {
                            uri: format!("file://{}", file_path.display()),
                            mime_type: Some("text/plain".to_string()),
                            text: content,
                            meta: None,
                        })),
                        annotations: None,
                    }));
                }
            }
        }

        let prompt_request = PromptRequest {
            session_id: session_info.acp_session_id.clone(),
            prompt: content_blocks,
            meta: Some(json!({
                "project_id": project_id,
                "action": "user_prompt"
            })),
        };

        let response = connection.prompt(prompt_request).await?;

        Ok(response)
    }

    /// List all projects
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        self.acp_client.project_manager.list_projects().await
    }

    /// Get a specific project
    pub async fn get_project(&self, project_id: Uuid) -> Result<Option<Project>> {
        self.acp_client.project_manager.get_project(project_id).await
    }

    /// Delete a project
    pub async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        info!("Deleting project: {}", project_id);

        // Cancel any ongoing sessions
        let sessions = self.acp_client.sessions.read().await;
        if let Some(session_info) = sessions.get(&project_id) {
            let connection = self.connections.read().await
                .get("default")
                .ok_or_else(|| anyhow::anyhow!("No ACP connection available"))?
                .clone();

            let cancel_notification = CancelNotification {
                session_id: session_info.acp_session_id.clone(),
                meta: None,
            };

            connection.cancel(cancel_notification).await?;
        }

        // Delete project
        self.acp_client.project_manager.delete_project(project_id).await?;

        // Remove session info
        let mut sessions = self.acp_client.sessions.write().await;
        sessions.remove(&project_id);

        Ok(())
    }
}

// ACP Client Implementation
impl AcpClientImpl {
    pub fn new(working_dir: PathBuf, project_manager: Arc<ProjectManager>) -> Self {
        Self {
            working_dir,
            project_manager,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            permission_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Client for AcpClientImpl {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        info!("Permission requested: {:?}", args);

        // Check permission cache first
        let cache_key = format!("{}:{}", args.session_id, args.tool_call.tool_call_id);
        let cache = self.permission_cache.read().await;

        if let Some(&granted) = cache.get(&cache_key) {
            return Ok(RequestPermissionResponse {
                outcome: if granted {
                    RequestPermissionOutcome::Selected {
                        option_id: PermissionOptionId(Arc::from("allow_once")),
                    }
                } else {
                    RequestPermissionOutcome::Selected {
                        option_id: PermissionOptionId(Arc::from("reject_once")),
                    }
                },
                meta: None,
            });
        }

        // For now, auto-approve file operations within project directories
        // In a real implementation, this would prompt the user
        let should_allow = args.tool_call.name == "write_text_file" ||
                          args.tool_call.name == "read_text_file";

        // Cache the decision
        let mut cache = self.permission_cache.write().await;
        cache.insert(cache_key, should_allow);

        Ok(RequestPermissionResponse {
            outcome: if should_allow {
                RequestPermissionOutcome::Selected {
                    option_id: PermissionOptionId(Arc::from("allow_once")),
                }
            } else {
                RequestPermissionOutcome::Selected {
                    option_id: PermissionOptionId(Arc::from("reject_once")),
                }
            },
            meta: None,
        })
    }

    async fn write_text_file(&self, args: WriteTextFileRequest) -> Result<WriteTextFileResponse, Error> {
        debug!("Writing file: {:?}", args.path);

        // Security check: ensure file is within working directory
        if !args.path.starts_with(&self.working_dir) {
            return Err(Error::invalid_params().with_message("File path outside working directory"));
        }

        // Create parent directories if they don't exist
        if let Some(parent) = args.path.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| Error::internal_error().with_message(&format!("Failed to create directory: {}", e)))?;
        }

        // Write the file
        tokio::fs::write(&args.path, &args.content).await
            .map_err(|e| Error::internal_error().with_message(&format!("Failed to write file: {}", e)))?;

        info!("File written successfully: {:?}", args.path);
        Ok(WriteTextFileResponse { meta: None })
    }

    async fn read_text_file(&self, args: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        debug!("Reading file: {:?}", args.path);

        // Security check: ensure file is within working directory
        if !args.path.starts_with(&self.working_dir) {
            return Err(Error::invalid_params().with_message("File path outside working directory"));
        }

        // Check if file exists
        if !args.path.exists() {
            return Err(Error::invalid_params().with_message("File not found"));
        }

        // Read the file
        let content = tokio::fs::read_to_string(&args.path).await
            .map_err(|e| Error::internal_error().with_message(&format!("Failed to read file: {}", e)))?;

        // Apply line and limit filters if specified
        let mut content = content;
        if let Some(line) = args.line {
            let lines: Vec<&str> = content.lines().collect();
            if line == 0 || line > lines.len() as u32 {
                return Err(Error::invalid_params().with_message("Invalid line number"));
            }

            let start_line = (line - 1) as usize;
            if let Some(limit) = args.limit {
                let end_line = std::cmp::min(start_line + limit as usize, lines.len());
                content = lines[start_line..end_line].join("\n");
            } else {
                content = lines[start_line..].join("\n");
            }
        } else if let Some(limit) = args.limit {
            let lines: Vec<&str> = content.lines().take(limit as usize).collect();
            content = lines.join("\n");
        }

        Ok(ReadTextFileResponse {
            content,
            meta: None,
        })
    }

    async fn session_notification(&self, args: SessionNotification) -> Result<(), Error> {
        debug!("Session notification: {:?}", args);

        // Handle different types of session updates
        match args.update {
            acp::SessionUpdate::UserMessageChunk { content } => {
                debug!("User message chunk: {:?}", content);
            }
            acp::SessionUpdate::AgentMessageChunk { content } => {
                debug!("Agent message chunk: {:?}", content);
            }
            acp::SessionUpdate::AgentThoughtChunk { content } => {
                debug!("Agent thought chunk: {:?}", content);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                info!("Tool call initiated: {:?}", tool_call);
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                info!("Tool call update: {:?}", update);
            }
            acp::SessionUpdate::Plan(plan) => {
                info!("Agent plan: {:?}", plan);
            }
            acp::SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                debug!("Available commands updated: {:?}", available_commands);
            }
            acp::SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                debug!("Current mode changed to: {}", current_mode_id);
            }
        }

        Ok(())
    }

    async fn create_terminal(&self, args: CreateTerminalRequest) -> Result<CreateTerminalResponse, Error> {
        info!("Creating terminal with command: {}", args.command);

        // For now, return a mock terminal ID
        // In a real implementation, this would create an actual terminal process
        let terminal_id = TerminalId(Arc::from(format!("term_{}", uuid::Uuid::new_v4())));

        Ok(CreateTerminalResponse {
            terminal_id,
            meta: None,
        })
    }

    async fn terminal_output(&self, args: TerminalOutputRequest) -> Result<TerminalOutputResponse, Error> {
        debug!("Getting terminal output for: {}", args.terminal_id);

        // Return empty output for now
        // In a real implementation, this would get actual terminal output
        Ok(TerminalOutputResponse {
            output: String::new(),
            truncated: false,
            exit_status: None,
            meta: None,
        })
    }

    async fn release_terminal(&self, args: ReleaseTerminalRequest) -> Result<ReleaseTerminalResponse, Error> {
        info!("Releasing terminal: {}", args.terminal_id);

        // In a real implementation, this would clean up the terminal process
        Ok(ReleaseTerminalResponse { meta: None })
    }

    async fn wait_for_terminal_exit(
        &self,
        args: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, Error> {
        debug!("Waiting for terminal exit: {}", args.terminal_id);

        // Return mock exit status
        // In a real implementation, this would wait for actual process exit
        Ok(WaitForTerminalExitResponse {
            exit_status: TerminalExitStatus {
                exit_code: Some(0),
                signal: None,
                meta: None,
            },
            meta: None,
        })
    }

    async fn kill_terminal_command(
        &self,
        args: KillTerminalCommandRequest,
    ) -> Result<KillTerminalCommandResponse, Error> {
        info!("Killing terminal command: {}", args.terminal_id);

        // In a real implementation, this would kill the actual process
        Ok(KillTerminalCommandResponse { meta: None })
    }

    async fn ext_method(&self, args: ExtRequest) -> Result<ExtResponse, Error> {
        warn!("Unhandled extension method: {}", args.method);
        Err(Error::method_not_found())
    }

    async fn ext_notification(&self, args: ExtNotification) -> Result<(), Error> {
        debug!("Extension notification: {}: {}", args.method, args.params);
        Ok(())
    }
}
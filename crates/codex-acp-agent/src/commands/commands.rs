use crate::{
    CodexAgent,
    agent::{APPROVAL_PRESETS, ClientOp, SessionState},
};

use agent_client_protocol::{self as acp, AvailableCommand, AvailableCommandInput, Error};
use codex_core::{
    NewConversation,
    protocol::{AskForApproval, InputItem, Op, ReviewRequest, SandboxPolicy},
};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::{fs, io};
use tokio::sync::oneshot;
use uuid::Uuid;

pub static AVAILABLE_COMMANDS: LazyLock<Vec<AvailableCommand>> = LazyLock::new(built_in_commands);

impl CodexAgent {
    pub async fn handle_slash_command(
        &self,
        session_id: &acp::SessionId,
        name: &str,
        _rest: &str,
    ) -> Result<(Option<Op>, bool), Error> {
        let conversation = self.get_conversation(session_id).await?;

        // Commands implemented inline (no Codex submission needed)
        match name {
            "new" => {
                // Start a new conversation within the current session
                let (conversation_id, conversation) = match self
                    .conversation_manager
                    .new_conversation(self.config.clone())
                    .await
                {
                    Ok(NewConversation {
                        conversation_id,
                        conversation,
                        ..
                    }) => (conversation_id, conversation),
                    Err(e) => {
                        self.send_message_chunk(
                            session_id,
                            format!("Failed to start new conversation: {}", e).into(),
                        )
                        .await?;
                        return Ok((None, true));
                    }
                };

                let current_mode = Self::modes(&self.config)
                    .as_ref()
                    .map(|m| m.current_mode_id.clone())
                    .unwrap_or(acp::SessionModeId("auto".into()));

                let fs_session_id = Uuid::new_v4().to_string();

                // Update the session with the new conversation
                self.sessions.borrow_mut().insert(
                    conversation_id.to_string(),
                    SessionState {
                        fs_session_id,
                        conversation_id: conversation_id.to_string(),
                        conversation: Some(conversation.clone()),
                        current_approval: self.config.approval_policy,
                        current_sandbox: self.config.sandbox_policy.clone(),
                        current_mode,
                        token_usage: None,
                        reasoning_sections: Vec::new(),
                        current_reasoning_chunk: String::new(),
                    },
                );

                self.send_message_chunk(session_id, "✨ Started a new conversation".into())
                    .await?;
                return Ok((None, true));
            }
            "status" => {
                let status_text = self.render_status(session_id).await;
                self.send_message_chunk(session_id, status_text.into())
                    .await?;
                return Ok((None, true));
            }
            "model" => {
                let rest = _rest.trim();
                if rest.is_empty() {
                    let msg = format!(
                        "Current model: {}\nUsage: /model <model-slug>",
                        self.config.model,
                    );
                    self.send_message_chunk(session_id, msg.into()).await?;
                    return Ok((None, true));
                }

                // Request Codex to change the model for subsequent turns.
                let op = Op::OverrideTurnContext {
                    cwd: None,
                    approval_policy: None,
                    sandbox_policy: None,
                    model: Some(rest.to_string()),
                    effort: None,
                    summary: None,
                };

                conversation
                    .submit(op)
                    .await
                    .map_err(Error::into_internal_error)?;

                // Provide immediate feedback to the user.
                self.send_message_chunk(
                    session_id,
                    format!("🧠 Requested model change to: `{}`", rest).into(),
                )
                .await?;
                return Ok((None, true));
            }
            "approvals" => {
                let mode = _rest.trim().to_lowercase();
                let allowed = ["read-only", "auto", "full-access"];

                if !allowed.contains(&mode.as_str()) {
                    let msg = format!("Usage: /approvals {}", allowed.join("|"));
                    self.send_message_chunk(session_id, msg.into()).await?;
                    return Ok((None, true));
                }

                let preset = APPROVAL_PRESETS
                    .iter()
                    .find(|preset| mode == preset.id)
                    .ok_or_else(Error::invalid_params)?;

                let submit_result = conversation
                    .submit(Op::OverrideTurnContext {
                        cwd: None,
                        approval_policy: Some(preset.approval),
                        sandbox_policy: Some(preset.sandbox.clone()),
                        model: None,
                        effort: None,
                        summary: None,
                    })
                    .await;

                if let Err(e) = submit_result {
                    self.send_message_chunk(
                        session_id,
                        format!("⚠️ Failed to set approval policy: {}", e).into(),
                    )
                    .await?;
                    return Ok((None, true));
                }

                // Persist our local view of the policy for /status
                self.with_session_state_mut(session_id, |state| {
                    state.current_approval = preset.approval;
                    state.current_sandbox = preset.sandbox.clone();
                    state.current_mode = acp::SessionModeId(preset.id.into());
                });

                let (tx, rx) = oneshot::channel();
                self.session_update_tx
                    .send((
                        acp::SessionNotification {
                            session_id: session_id.clone(),
                            update: acp::SessionUpdate::CurrentModeUpdate {
                                current_mode_id: acp::SessionModeId(preset.id.into()),
                            },
                            meta: None,
                        },
                        tx,
                    ))
                    .map_err(Error::into_internal_error)?;
                rx.await.map_err(Error::into_internal_error)?;
                return Ok((None, true));
            }
            "quit" => {
                // Say goodbye and submit Shutdown to Codex if available
                let mut quit_msg = "👋 Codex agent is shutting down. Goodbye!".into();
                // Request backend shutdown
                if let Err(e) = conversation.submit(Op::Shutdown).await {
                    quit_msg = format!("Failed to submit shutdown: {}", e);
                }

                // Send the goodbye message
                self.send_message_chunk(session_id, quit_msg.into()).await?;
                return Ok((None, true));
            }
            _ => {}
        }

        let mut msg = String::default();
        // Commands forwarded to Codex as protocol Ops
        let op = match name {
            "init" => {
                let prompt = r#"
Generate a file named AGENTS.md that serves as a contributor guide for this repository.
Your goal is to produce a clear, concise, and well-structured document with descriptive headings and actionable explanations for each section.
Follow the outline below, but adapt as needed — add sections if relevant, and omit those that do not apply to this project.

Document Requirements

- Title the document "Repository Guidelines".
- Use Markdown headings (#, ##, etc.) for structure.
- Keep the document concise. 200-400 words is optimal.
- Keep explanations short, direct, and specific to this repository.
- Provide examples where helpful (commands, directory paths, naming patterns).
- Maintain a professional, instructional tone.

Recommended Sections

Project Structure & Module Organization

- Outline the project structure, including where the source code, tests, and assets are located.

Build, Test, and Development Commands

- List key commands for building, testing, and running locally (e.g., npm test, make build).
- Briefly explain what each command does.

Coding Style & Naming Conventions

- Specify indentation rules, language-specific style preferences, and naming patterns.
- Include any formatting or linting tools used.

Testing Guidelines

- Identify testing frameworks and coverage requirements.
- State test naming conventions and how to run tests.

Commit & Pull Request Guidelines

- Summarize commit message conventions found in the project’s Git history.
- Outline pull request requirements (descriptions, linked issues, screenshots, etc.).

(Optional) Add other sections if relevant, such as Security & Configuration Tips, Architecture Overview, or Agent-Specific Instructions.
"#;
                msg = "📝 Creating AGENTS.md file with initial instructions...\n\n".into();
                Some(Op::UserInput {
                    items: vec![InputItem::Text {
                        text: prompt.into(),
                    }],
                })
            }
            "compact" => {
                self.with_session_state_mut(session_id, |state| {
                    state.token_usage = None;
                });
                msg = "🧠 Compacting conversation to reduce context size...\n\n".into();
                Some(Op::Compact)
            }
            "review" => {
                msg = "🔍 Asking Codex to review current changes...\n\n".into();
                Some(Op::Review {
                    review_request: ReviewRequest {
                        prompt: "review current changes".to_string(),
                        user_facing_hint: "current changes".to_string(),
                    },
                })
            }
            _ => None,
        };

        if !msg.is_empty() {
            self.send_message_chunk(session_id, msg.into()).await?;
        }

        Ok((op, false))
    }

    async fn render_status(&self, session_id: &acp::SessionId) -> String {
        let sid_str = session_id.0.as_ref();
        // Session snapshot
        let (approval_mode, sandbox_mode, token_usage) = {
            if let Some(state) = self.sessions.borrow().get(sid_str) {
                (
                    state.current_approval,
                    state.current_sandbox.clone(),
                    state.token_usage.clone(),
                )
            } else {
                (
                    AskForApproval::OnRequest,
                    SandboxPolicy::new_workspace_write_policy(),
                    None,
                )
            }
        };

        // Workspace
        let cwd = self.shorten_home(&self.config.cwd);
        let agents_files = self.find_agents_files(Some(session_id)).await;
        let agents_line = if agents_files.is_empty() {
            "(none)".to_string()
        } else {
            agents_files
                .iter()
                .map(|f| self.shorten_home(&self.config.cwd.join(f)))
                .collect::<Vec<_>>()
                .join(", ")
        };

        // Account
        let (auth_mode, email, plan): (String, String, String) =
            match self.auth_manager.read().ok().and_then(|am| am.auth()) {
                Some(auth) => match auth.get_token_data().await {
                    Ok(td) => {
                        let email = td
                            .id_token
                            .email
                            .clone()
                            .unwrap_or_else(|| "(none)".to_string());
                        let plan = td
                            .id_token
                            .get_chatgpt_plan_type()
                            .unwrap_or_else(|| "(unknown)".to_string());
                        ("ChatGPT".to_string(), email, plan)
                    }
                    Err(_) => (
                        "API key".to_string(),
                        "(none)".to_string(),
                        "(unknown)".to_string(),
                    ),
                },
                None => (
                    "Not signed in".to_string(),
                    "(none)".to_string(),
                    "(unknown)".to_string(),
                ),
            };

        // Model
        let model = &self.config.model;
        let provider = self.title_case(&self.config.model_provider_id);
        let effort = self.title_case(
            format!("{}", self.config.model_reasoning_effort.unwrap_or_default()).as_str(),
        );
        let summary = self.title_case(format!("{}", self.config.model_reasoning_summary).as_str());

        // Tokens
        let (input, output, total) = match token_usage {
            Some(u) => (
                u.input_tokens.to_string(),
                u.output_tokens.to_string(),
                u.total_tokens.to_string(),
            ),
            None => ("0".to_string(), "0".to_string(), "0".to_string()),
        };

        let status = format!(
            r#"
📂 Workspace

    Path:          {cwd}
    Approval Mode: {approval}
    Sandbox:       {sandbox}
    AGENTS files:  {agents}

👤 Account

    Signed in with: {auth_mode}
    Login:          {email}
    Plan:           {plan}

🧠 Model

    Name:                {model}
    Provider:            {provider}
    Reasoning Effort:    {effort}
    Reasoning Summaries: {summary}

📊 Token Usage

    Session ID:     {sid}
    Input:          {input}
    Output:         {output}
    Total:          {total}
"#,
            cwd = cwd,
            approval = approval_mode,
            sandbox = sandbox_mode,
            agents = agents_line,
            auth_mode = auth_mode,
            email = email,
            plan = plan,
            model = model,
            provider = provider,
            effort = effort,
            summary = summary,
            sid = sid_str,
            input = input,
            output = output,
            total = total,
        );
        status
    }

    fn shorten_home(&self, p: &Path) -> String {
        let s = p.display().to_string();
        if let Ok(home) = std::env::var("HOME")
            && s.starts_with(&home)
        {
            return s.replacen(&home, "~", 1);
        }
        s
    }

    async fn find_agents_files(&self, session_id: Option<&acp::SessionId>) -> Vec<String> {
        let mut names = Vec::new();
        let candidates = ["AGENTS.md", "Agents.md", "agents.md"];

        for candidate in candidates.iter() {
            let path = self.config.cwd.join(candidate);
            let mut found = false;

            if let Some(session_id) = session_id
                && self.client_supports_fs_read()
                && self
                    .client_read_text_file(session_id, path.clone(), Some(1), Some(1))
                    .await
                    .is_ok()
            {
                found = true;
            }

            if !found && path.exists() {
                found = true;
            }

            if found {
                names.push((*candidate).to_string());
            }
        }

        names
    }

    fn client_supports_fs_read(&self) -> bool {
        self.client_capabilities.borrow().fs.read_text_file
    }

    #[allow(dead_code)]
    fn client_supports_fs_write(&self) -> bool {
        self.client_capabilities.borrow().fs.write_text_file
    }

    async fn client_read_text_file(
        &self,
        session_id: &acp::SessionId,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<acp::ReadTextFileResponse, Error> {
        let (tx, rx) = oneshot::channel();
        let request = acp::ReadTextFileRequest {
            session_id: session_id.clone(),
            path,
            line,
            limit,
            meta: None,
        };
        self.client_tx
            .send(ClientOp::ReadTextFile(request, tx))
            .map_err(|_| {
                Error::internal_error().with_data("client read_text_file channel closed")
            })?;
        rx.await.map_err(|_| {
            Error::internal_error().with_data("client read_text_file response dropped")
        })?
    }

    #[allow(dead_code)]
    async fn client_write_text_file(
        &self,
        session_id: &acp::SessionId,
        path: PathBuf,
        content: String,
    ) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        let request = acp::WriteTextFileRequest {
            session_id: session_id.clone(),
            path,
            content,
            meta: None,
        };
        self.client_tx
            .send(ClientOp::WriteTextFile(request, tx))
            .map_err(|_| {
                Error::internal_error().with_data("client write_text_file channel closed")
            })?;
        let response = rx.await.map_err(|_| {
            Error::internal_error().with_data("client write_text_file response dropped")
        })?;
        response.map(|_| ())
    }

    #[allow(dead_code)]
    fn write_text_file_locally(&self, path: &Path, content: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)
    }

    fn title_case(&self, s: &str) -> String {
        if s.is_empty() {
            return s.to_string();
        }
        let mut chars = s.chars();
        let first = chars.next().unwrap().to_uppercase().to_string();
        let rest = chars.as_str();
        format!("{}{}", first, rest)
    }
}

fn built_in_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand {
            name: "new".into(),
            description: "start a new chat during a conversation".into(),
            input: None,
            meta: None,
        },
        AvailableCommand {
            name: "init".into(),
            description: "create an AGENTS.md file with instructions for Codex".into(),
            input: None,
            meta: None,
        },
        AvailableCommand {
            name: "compact".into(),
            description: "summarize conversation to prevent hitting the context limit".into(),
            input: None,
            meta: None,
        },
        AvailableCommand {
            name: "review".into(),
            description: "review my current changes and find issues".to_string(),
            input: None,
            meta: None,
        },
        AvailableCommand {
            name: "model".into(),
            description: "choose what model and reasoning effort to use".into(),
            input: Some(AvailableCommandInput::Unstructured {
                hint: "Model slug, e.g., gpt-codex".into(),
            }),
            meta: None,
        },
        AvailableCommand {
            name: "approvals".into(),
            description: "choose what Codex can do without approval".into(),
            input: Some(AvailableCommandInput::Unstructured {
                hint: "read-only|auto|full-access".into(),
            }),
            meta: None,
        },
        AvailableCommand {
            name: "status".into(),
            description: "show current session configuration and token usage".into(),
            input: None,
            meta: None,
        },
        AvailableCommand {
            name: "quit".into(),
            description: "exit Codex".into(),
            input: None,
            meta: None,
        },
    ]
}

use super::*;
use agent_client_protocol::{AvailableCommand, AvailableCommandInput};
use std::fs;
use tokio::sync::oneshot;

impl ClaudeAgent {
    pub fn built_in_commands() -> Vec<AvailableCommand> {
        vec![
            AvailableCommand {
                name: "init".into(),
                description: "create a CLAUDE.md file with instructions for Claude".into(),
                input: None,
                meta: None,
            },
            AvailableCommand {
                name: "model".into(),
                description: "choose what model to use".into(),
                input: Some(AvailableCommandInput::Unstructured {
                    hint: "Model slug, e.g., claude-3-5-sonnet-20241022".into(),
                }),
                meta: None,
            },
            AvailableCommand {
                name: "approvals".into(),
                description: "choose what Claude can do without approval".into(),
                input: Some(AvailableCommandInput::Unstructured {
                    hint: "on-request|on-failure|never|unless-trusted".into(),
                }),
                meta: None,
            },
            AvailableCommand {
                name: "status".into(),
                description: "show current session configuration and token usage".into(),
                input: None,
                meta: None,
            },
        ]
    }

    pub fn available_commands(&self) -> Vec<AvailableCommand> {
        let mut cmds = Self::built_in_commands();
        cmds.extend(self.extra_available_commands.borrow().iter().cloned());
        cmds
    }

    pub async fn handle_slash_command(
        &self,
        session_id: &SessionId,
        name: &str,
        _rest: &str,
    ) -> Result<bool, Error> {
        let sid_str = session_id.0.to_string();
        let _session = match self.sessions.borrow().get(&sid_str) {
            Some(s) => s.clone(),
            None => return Err(Error::invalid_params()),
        };

        // Commands implemented inline (no Claude submission needed)
        match name {
            "init" => {
                // Create CLAUDE.md in the current workspace if it doesn't already exist.
                let rest = _rest.trim();
                let force = matches!(rest, "--force" | "-f" | "force");

                let cwd = self.config.cwd.clone();
                // If any CLAUDE* file already exists and not forcing, bail out.
                let existing = self.find_claude_files();
                if !existing.is_empty() && !force {
                    let msg = format!(
                        "CLAUDE file already exists: {}\nUse /init --force to overwrite.",
                        existing.join(", ")
                    );

                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(session_id, msg.into(), tx)?;
                    let _ = rx.await;
                    return Ok(true);
                }

                let target = cwd.join("CLAUDE.md");
                let template = r#"# CLAUDE.md

This file gives Claude instructions for working in this repository. Place project-specific tips here so the agent acts consistently with your workflows.

Scope
- The scope of this file is the entire repository (from this folder down).
- Add more CLAUDE.md files in subdirectories for overrides; deeper files take precedence.

Coding Conventions
- Keep changes minimal and focused on the task.
- Match the existing code style and structure; avoid wholesale refactors.
- Don't add licenses or headers unless requested.

Workflow
- How to run and test: describe commands (e.g., `cargo test`, `npm test`).
- Any environment variables or secrets required for local runs.
- Where to place new modules, configs, or scripts.

Reviews and Safety
- Point out risky or destructive actions before performing them.
- Prefer root-cause fixes over band-aids.
- When in doubt, ask for confirmation.

Notes for Agents
- Follow instructions in this file for all edits within its scope.
- Files in deeper directories with their own CLAUDE.md override these rules.
"#;

                // Try to write the file; on errors, surface a message.
                let result = (|| -> std::io::Result<()> {
                    // Ensure parent exists (workspace root should exist already).
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&target, template)
                })();

                let msg = match result {
                    Ok(()) => format!(
                        "Initialized CLAUDE.md at {}\nEdit it to customize agent behavior.",
                        self.shorten_home(&target)
                    ),
                    Err(e) => format!(
                        "Failed to create CLAUDE.md: {}\nPath: {}",
                        e,
                        self.shorten_home(&target)
                    ),
                };

                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(session_id, msg.into(), tx)?;
                let _ = rx.await;
                return Ok(true);
            }
            "status" => {
                let status_text = self.render_status(&sid_str).await;
                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(session_id, status_text.into(), tx)?;
                let _ = rx.await;
                return Ok(true);
            }
            "model" => {
                let rest = _rest.trim();
                if rest.is_empty() {
                    let msg = format!(
                        "Current model: {}\nUsage: /model <model-slug>",
                        self.config.model,
                    );
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(session_id, msg.into(), tx)?;
                    let _ = rx.await;
                    return Ok(true);
                }

                // Update model configuration
                let msg = format!("Requested model change to: {}", rest);
                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(session_id, msg.into(), tx)?;
                let _ = rx.await;
                return Ok(true);
            }
            "approvals" => {
                let value = _rest.trim().to_lowercase();
                let parsed = match value.as_str() {
                    "" | "show" => None,
                    "on-request" => Some(ApprovalPolicy::OnRequest),
                    "on-failure" => Some(ApprovalPolicy::OnFailure),
                    "never" => Some(ApprovalPolicy::Never),
                    "unless-trusted" | "untrusted" => Some(ApprovalPolicy::UnlessTrusted),
                    _ => {
                        let msg = "Usage: /approvals on-request|on-failure|never|unless-trusted";
                        let (tx, rx) = oneshot::channel();
                        self.send_message_chunk(session_id, msg.into(), tx)?;
                        let _ = rx.await;
                        return Ok(true);
                    }
                };

                if let Some(policy) = parsed {
                    // Persist our local view of the policy for /status
                    if let Ok(mut map) = self.sessions.try_borrow_mut()
                        && let Some(state) = map.get_mut(&sid_str)
                    {
                        state.current_approval = policy;
                    }
                    let msg = format!("Approval policy set to: {}", value);
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(session_id, msg.into(), tx)?;
                    let _ = rx.await;
                } else {
                    // show current (best-effort from config)
                    let msg = "Current approval policy: configured per session. Use /approvals <policy> to set.";
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(session_id, msg.into(), tx)?;
                    let _ = rx.await;
                }
                return Ok(true);
            }
            _ => {}
        }

        // Commands that would be forwarded to Claude (placeholder implementation)
        match name {
            "help" => {
                let msg = "Available commands:\n  /init - Create CLAUDE.md\n  /model - Change model\n  /approvals - Set approval policy\n  /status - Show session status";
                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(session_id, msg.into(), tx)?;
                let _ = rx.await;
                return Ok(true);
            }
            _ => {}
        }

        Ok(false)
    }

    async fn render_status(&self, sid_str: &str) -> String {
        // Session snapshot
        let (approval_mode, token_usage, session_uuid) = {
            let map = self.sessions.borrow();
            if let Some(state) = map.get(sid_str) {
                (
                    state.current_approval.clone(),
                    state.token_usage.clone(),
                    state.conversation_id.clone(),
                )
            } else {
                (
                    ApprovalPolicy::default(),
                    None,
                    String::new(),
                )
            }
        };

        // Workspace
        let cwd = self.shorten_home(&self.config.cwd);
        let claude_files = self.find_claude_files();
        let claude_line = if claude_files.is_empty() {
            "(none)".to_string()
        } else {
            claude_files.join(", ")
        };

        // Account
        let api_key_set = std::env::var("CLAUDE_API_KEY").is_ok();
        let auth_status = if api_key_set {
            "API Key configured"
        } else {
            "Not configured"
        };

        // Model
        let model = &self.config.model;

        // Tokens
        let (input, output, total) = match token_usage {
            Some(u) => (u.input_tokens, u.output_tokens, u.total_tokens),
            None => (0, 0, 0),
        };

        format!(
            "📂 Workspace\n  • Path: {cwd}\n  • Approval Mode: {approval}\n  • CLAUDE files: {claude}\n\n🔐 Authentication\n  • Status: {auth}\n\n🧠 Model\n  • Name: {model}\n\n📊 Token Usage\n  • Session ID: {sid}\n  • Input: {input}\n  • Output: {output}\n  • Total: {total}",
            cwd = cwd,
            approval = format!("{:?}", approval_mode),
            claude = claude_line,
            auth = auth_status,
            model = model,
            sid = session_uuid,
            input = input,
            output = output,
            total = total,
        )
    }

    fn shorten_home(&self, p: &std::path::Path) -> String {
        let s = p.display().to_string();
        if let Ok(home) = std::env::var("HOME")
            && s.starts_with(&home)
        {
            return s.replacen(&home, "~", 1);
        }
        s
    }

    fn find_claude_files(&self) -> Vec<String> {
        let mut names = Vec::new();
        let candidates = ["CLAUDE.md", "Claude.md", "claude.md"];
        for c in candidates.iter() {
            let path = self.config.cwd.join(c);
            if path.exists() {
                names.push(c.to_string());
            }
        }
        names
    }
}
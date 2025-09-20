use super::*;
use agent_client_protocol::{AvailableCommand, AvailableCommandInput};
use std::{fs, io};
use tokio::sync::oneshot;

impl CodexAgent {
    pub fn built_in_commands() -> Vec<AvailableCommand> {
        vec![
            AvailableCommand {
                name: "init".into(),
                description: "create an AGENTS.md file with instructions for Codex".into(),
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
                    hint: "untrusted|on-request|on-failure|never".into(),
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
        let session = match self.sessions.borrow().get(&sid_str) {
            Some(s) => s.clone(),
            None => return Err(Error::invalid_params()),
        };

        // Commands implemented inline (no Codex submission needed)
        match name {
            "init" => {
                // Create AGENTS.md in the current workspace if it doesn't already exist.
                let rest = _rest.trim();
                let force = matches!(rest, "--force" | "-f" | "force");

                let cwd = self.config.cwd.clone();
                // If any AGENTS* file already exists and not forcing, bail out.
                let existing = self.find_agents_files();
                if !existing.is_empty() && !force {
                    let msg = format!(
                        "AGENTS file already exists: {}\nUse /init --force to overwrite.",
                        existing.join(", ")
                    );

                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(session_id, msg.into(), tx)?;
                    let _ = rx.await;
                    return Ok(true);
                }

                let target = cwd.join("AGENTS.md");
                let template = r#"# AGENTS.md

This file gives Codex instructions for working in this repository. Place project-specific tips here so the agent acts consistently with your workflows.

Scope
- The scope of this file is the entire repository (from this folder down).
- Add more AGENTS.md files in subdirectories for overrides; deeper files take precedence.

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
- Files in deeper directories with their own AGENTS.md override these rules.
"#;

                // Try to write the file; on errors, surface a message.
                let result = (|| -> io::Result<()> {
                    // Ensure parent exists (workspace root should exist already).
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&target, template)
                })();

                let msg = match result {
                    Ok(()) => format!(
                        "Initialized AGENTS.md at {}\nEdit it to customize agent behavior.",
                        self.shorten_home(&target)
                    ),
                    Err(e) => format!(
                        "Failed to create AGENTS.md: {}\nPath: {}",
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

                // In ACP adapter mode, we don't have direct model switching
                // This would need to be implemented through the adapter
                let msg = format!("Model change requested to: {}. Note: Model switching in ACP adapter mode is not yet implemented.", rest);
                let (tx, rx) = oneshot::channel();
                self.send_message_chunk(session_id, msg.into(), tx)?;
                let _ = rx.await;
                return Ok(true);
            }
            "approvals" => {
                let value = _rest.trim().to_lowercase();
                match value.as_str() {
                    "" | "show" => {
                        let msg = "Current approval policy: configured per session. Use /approvals <policy> to set.";
                        let (tx, rx) = oneshot::channel();
                        self.send_message_chunk(session_id, msg.into(), tx)?;
                        let _ = rx.await;
                    }
                    "on-request" | "on-failure" | "never" | "untrusted" | "unless-trusted" => {
                        let msg = format!("Approval policy set to: {}. Note: Policy changes in ACP adapter mode are not yet implemented.", value);
                        let (tx, rx) = oneshot::channel();
                        self.send_message_chunk(session_id, msg.into(), tx)?;
                        let _ = rx.await;
                    }
                    _ => {
                        let msg = "Usage: /approvals untrusted|on-request|on-failure|never";
                        let (tx, rx) = oneshot::channel();
                        self.send_message_chunk(session_id, msg.into(), tx)?;
                        let _ = rx.await;
                    }
                }
                return Ok(true);
            }
            _ => {}
        }

        // Commands that would require ACP adapter implementation
        match name {
            "compact" | "list-tools" | "tools" | "list-custom-prompts" | "prompts" | "history" | "shutdown" => {
                let msg = format!("Command '{}' is not yet implemented in ACP adapter mode", name);
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
        let (token_usage, session_uuid) = {
            let map = self.sessions.borrow();
            if let Some(state) = map.get(sid_str) {
                (
                    state.token_usage,
                    state.conversation_id.clone(),
                )
            } else {
                (
                    None,
                    String::new(),
                )
            }
        };

        // Workspace
        let cwd = self.shorten_home(&self.config.cwd);
        let agents_files = self.find_agents_files();
        let agents_line = if agents_files.is_empty() {
            "(none)".to_string()
        } else {
            agents_files.join(", ")
        };

        // Account - simplified for ACP adapter mode
        let (auth_mode, email, plan): (String, String, String) =
            if std::env::var("OPENAI_API_KEY").is_ok() {
                ("API Key".to_string(), "(configured)".to_string(), "(unknown)".to_string())
            } else {
                ("Not configured".to_string(), "(none)".to_string(), "(none)".to_string())
            };

        // Model - from config
        let model = &self.config.model;
        let provider = "ACP Adapter".to_string();
        let effort = "Default".to_string();
        let summary = "Enabled".to_string();

        // Tokens
        let (input, output, total) = match token_usage {
            Some(u) => (u, u, u), // Simplified token tracking
            None => (0, 0, 0),
        };

        format!(
            "📂 Workspace\n  • Path: {cwd}\n  • AGENTS files: {agents}\n\n👤 Account\n  • Auth Mode: {auth_mode}\n  • Login: {email}\n  • Plan: {plan}\n\n🧠 Model\n  • Name: {model}\n  • Provider: {provider}\n  • Reasoning Effort: {effort}\n  • Reasoning Summaries: {summary}\n\n📊 Token Usage\n  • Session ID: {sid}\n  • Input: {input}\n  • Output: {output}\n  • Total: {total}\n\n🔧 Adapter\n  • Mode: ACP Adapter\n  • Status: Active",
            cwd = cwd,
            agents = agents_line,
            auth_mode = auth_mode,
            email = email,
            plan = plan,
            model = model,
            provider = provider,
            effort = effort,
            summary = summary,
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

    fn find_agents_files(&self) -> Vec<String> {
        let mut names = Vec::new();
        let candidates = ["AGENTS.md", "Agents.md", "agents.md"];
        for c in candidates.iter() {
            let path = self.config.cwd.join(c);
            if path.exists() {
                names.push(c.to_string());
            }
        }
        names
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
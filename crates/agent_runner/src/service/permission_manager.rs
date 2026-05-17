use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use agent_abstraction::{PermissionRequestContext, PermissionRequestHandler};
use agent_client_protocol::Responder;
use agent_client_protocol::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::Mutex;
use shared_types::{
    AcpRequestPermission, AgentMode, ResolvePermissionRequestDto, ResolvePermissionResponseDto,
    SessionNotify,
};
use tracing::{error, info, warn};

use super::push_session_update_with_project;

const PENDING_TIMEOUT: Duration = Duration::from_secs(300);

pub static PERMISSION_MANAGER: LazyLock<Arc<PermissionManager>> =
    LazyLock::new(|| Arc::new(PermissionManager::default()));

type PendingKey = (String, String);
type RuleKey = (String, String, String);

struct PendingPermission {
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    context: PermissionRequestContext,
    created_at: Instant,
    save_rule: Option<SaveRuleSuggestion>,
}

#[derive(Debug, Clone)]
struct SaveRuleSuggestion {
    tool_name: String,
    pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
struct PermissionRule {
    decision: RuleDecision,
    /// Stored for debugging/inspection; the active engine is `compiled`.
    #[allow(dead_code)]
    pattern: String,
    /// Compiled regex, created once at insertion time.
    compiled: Option<regex::Regex>,
}

pub struct PermissionManager {
    pending: Mutex<HashMap<PendingKey, PendingPermission>>,
    rules: DashMap<RuleKey, Vec<PermissionRule>>,
}

impl Default for PermissionManager {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            rules: DashMap::new(),
        }
    }
}

impl PermissionManager {
    pub async fn resolve_permission(
        &self,
        input: ResolvePermissionRequestDto,
    ) -> ResolvePermissionResponseDto {
        let session_id = input.session_id.trim().to_string();
        let tool_call_id = input.tool_call_id.trim().to_string();
        let key = (session_id.clone(), tool_call_id.clone());

        let Some(pending) = self.pending.lock().remove(&key) else {
            return ResolvePermissionResponseDto {
                success: false,
                session_id,
                tool_call_id,
                outcome_json: None,
                rule_saved: false,
                error_code: Some(shared_types::error_codes::ERR_PERMISSION_NOT_FOUND.to_string()),
                message: Some("permission request not found or already resolved".to_string()),
            };
        };

        if pending.created_at.elapsed() > PENDING_TIMEOUT {
            let response = cancelled_response();
            let outcome_json = serde_json::to_string(&response).ok();
            if let Err(err) = pending.responder.respond(response) {
                error!("[Permission] failed to respond expired permission: {err}");
            }
            return ResolvePermissionResponseDto {
                success: false,
                session_id,
                tool_call_id,
                outcome_json,
                rule_saved: false,
                error_code: Some(shared_types::error_codes::ERR_PERMISSION_EXPIRED.to_string()),
                message: Some("permission request expired".to_string()),
            };
        }

        if let Some(project_id) = input.project_id.as_deref().filter(|s| !s.trim().is_empty())
            && project_id != pending.context.project_id
        {
            self.pending.lock().insert(key, pending);
            return ResolvePermissionResponseDto {
                success: false,
                session_id,
                tool_call_id,
                outcome_json: None,
                rule_saved: false,
                error_code: Some(
                    shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED.to_string(),
                ),
                message: Some("project_id does not match pending permission".to_string()),
            };
        }

        if let Some(user_id) = input.user_id.as_deref().filter(|s| !s.trim().is_empty())
            && pending.context.user_id.as_deref() != Some(user_id)
        {
            self.pending.lock().insert(key, pending);
            return ResolvePermissionResponseDto {
                success: false,
                session_id,
                tool_call_id,
                outcome_json: None,
                rule_saved: false,
                error_code: Some(
                    shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED.to_string(),
                ),
                message: Some("user_id does not match pending permission".to_string()),
            };
        }

        let response = if input.cancelled {
            cancelled_response()
        } else {
            match input.option_id.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(option_id) => {
                    let option_id = option_id.trim().to_string();
                    if !pending
                        .request
                        .options
                        .iter()
                        .any(|option| option.option_id.to_string() == option_id)
                    {
                        self.pending.lock().insert(key, pending);
                        return ResolvePermissionResponseDto {
                            success: false,
                            session_id,
                            tool_call_id,
                            outcome_json: None,
                            rule_saved: false,
                            error_code: Some(
                                shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED
                                    .to_string(),
                            ),
                            message: Some(
                                "option_id is not available for this permission request"
                                    .to_string(),
                            ),
                        };
                    }

                    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                        SelectedPermissionOutcome::new(option_id),
                    ))
                }
                None => {
                    self.pending.lock().insert(key, pending);
                    return ResolvePermissionResponseDto {
                        success: false,
                        session_id,
                        tool_call_id,
                        outcome_json: None,
                        rule_saved: false,
                        error_code: Some(
                            shared_types::error_codes::ERR_VALIDATION.to_string(),
                        ),
                        message: Some(
                            "option_id is required when cancelled is false"
                                .to_string(),
                        ),
                    };
                }
            }
        };

        let selected_kind = match &response.outcome {
            RequestPermissionOutcome::Selected(selected) => pending
                .request
                .options
                .iter()
                .find(|option| option.option_id.to_string() == selected.option_id.to_string())
                .map(|option| option.kind),
            RequestPermissionOutcome::Cancelled => None,
            _ => None,
        };

        let mut rule_saved = false;
        if input.save_rule {
            if let (Some(suggestion), Some(kind)) = (&pending.save_rule, selected_kind) {
                rule_saved = self.save_rule_from_option_kind(&pending.context, suggestion, kind);
            }
        }

        let outcome_json = serde_json::to_string(&response).ok();
        match pending.responder.respond(response) {
            Ok(()) => ResolvePermissionResponseDto {
                success: true,
                session_id,
                tool_call_id,
                outcome_json,
                rule_saved,
                error_code: None,
                message: None,
            },
            Err(err) => ResolvePermissionResponseDto {
                success: false,
                session_id,
                tool_call_id,
                outcome_json,
                rule_saved,
                error_code: Some(
                    shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED.to_string(),
                ),
                message: Some(err.to_string()),
            },
        }
    }

    pub fn cancel_session_permissions(&self, session_id: &str) -> usize {
        let keys: Vec<_> = self
            .pending
            .lock()
            .iter()
            .filter(|(key, _pending)| key.0 == session_id)
            .map(|(key, _pending)| key.clone())
            .collect();
        self.cancel_keys(keys)
    }

    pub fn cancel_project_permissions(&self, project_id: &str) -> usize {
        let keys: Vec<_> = self
            .pending
            .lock()
            .iter()
            .filter(|(_key, pending)| pending.context.project_id == project_id)
            .map(|(key, _pending)| key.clone())
            .collect();
        self.cancel_keys(keys)
    }

    fn cancel_keys(&self, keys: Vec<PendingKey>) -> usize {
        let mut count = 0;
        for key in keys {
            if let Some(pending) = self.pending.lock().remove(&key) {
                if let Err(err) = pending.responder.respond(cancelled_response()) {
                    warn!("[Permission] failed to cancel pending permission: {err}");
                }
                count += 1;
            }
        }
        count
    }

    fn store_pending(&self, key: PendingKey, pending: PendingPermission) {
        self.pending.lock().insert(key.clone(), pending);
        // PERMISSION_MANAGER is the single global instance (LazyLock<Arc<PermissionManager>>).
        // It is safe to capture via Arc clone here because:
        // 1. There is exactly one instance in the agent_runner process.
        // 2. The spawned task accesses the same `pending` map as the one
        //    the caller inserted into via `&self`.
        let manager = PERMISSION_MANAGER.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_TIMEOUT).await;
            let pending = manager.pending.lock().remove(&key);
            if let Some(pending) = pending {
                warn!(
                    "[Permission] permission request expired: session_id={}, tool_call_id={}",
                    key.0, key.1
                );
                if let Err(err) = pending.responder.respond(cancelled_response()) {
                    warn!("[Permission] failed to respond expired permission: {err}");
                }
            }
        });
    }

    fn save_rule_from_option_kind(
        &self,
        context: &PermissionRequestContext,
        suggestion: &SaveRuleSuggestion,
        kind: PermissionOptionKind,
    ) -> bool {
        let decision = match kind {
            PermissionOptionKind::AllowAlways => RuleDecision::Allow,
            PermissionOptionKind::RejectAlways => RuleDecision::Deny,
            _ => return false,
        };

        let user_key = context.user_id.clone().unwrap_or_default();
        let key = (
            context.project_id.clone(),
            user_key,
            suggestion.tool_name.clone(),
        );
        let compiled = regex::Regex::new(&suggestion.pattern).ok();
        let mut rules = self.rules.entry(key).or_default();
        rules.push(PermissionRule {
            decision,
            pattern: suggestion.pattern.clone(),
            compiled,
        });
        true
    }

    fn rule_decision(
        &self,
        context: &PermissionRequestContext,
        tool_name: &str,
        command: Option<&str>,
    ) -> Option<RuleDecision> {
        let command = command?;
        let user_key = context.user_id.clone().unwrap_or_default();
        let keys = [
            (context.project_id.clone(), user_key, tool_name.to_string()),
            (
                context.project_id.clone(),
                String::new(),
                tool_name.to_string(),
            ),
        ];

        let mut allow = false;
        for key in keys {
            if let Some(rules) = self.rules.get(&key) {
                for rule in rules.iter() {
                    if command_matches_pattern(command, rule) {
                        if rule.decision == RuleDecision::Deny {
                            return Some(RuleDecision::Deny);
                        }
                        allow = true;
                    }
                }
            }
        }

        allow.then_some(RuleDecision::Allow)
    }
}

#[async_trait]
impl PermissionRequestHandler for PermissionManager {
    async fn handle_permission_request(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        let session_id = request.session_id.to_string();
        let tool_name = extract_tool_name(&request);
        let command = extract_command(&request);

        if is_dangerous_command(command.as_deref()) {
            info!(
                "[Permission] auto reject dangerous command: session_id={}, tool_call_id={}, command={:?}",
                session_id, tool_call_id, command
            );
            return respond_with_preferred_option(
                &request,
                responder,
                &[
                    PermissionOptionKind::RejectAlways,
                    PermissionOptionKind::RejectOnce,
                ],
            );
        }

        if let Some(decision) = self.rule_decision(&context, &tool_name, command.as_deref()) {
            let preferred = match decision {
                RuleDecision::Allow => [
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::AllowOnce,
                ],
                RuleDecision::Deny => [
                    PermissionOptionKind::RejectAlways,
                    PermissionOptionKind::RejectOnce,
                ],
            };
            return respond_with_preferred_option(&request, responder, &preferred);
        }

        if context.agent_mode == AgentMode::Yolo {
            return respond_with_preferred_option(
                &request,
                responder,
                &[
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::AllowOnce,
                ],
            );
        }

        let save_rule = build_save_rule_suggestion(&tool_name, command.as_deref());
        let request_json = serde_json::to_value(&request).unwrap_or_else(|_| serde_json::json!({}));
        let save_rule_json = save_rule.as_ref().map(|suggestion| {
            serde_json::json!({
                "suggested_pattern": suggestion.pattern,
                "rule_type": "allow",
                "tool_name": suggestion.tool_name,
            })
        });

        let pending = PendingPermission {
            request,
            responder,
            context: context.clone(),
            created_at: Instant::now(),
            save_rule,
        };
        self.store_pending((session_id.clone(), tool_call_id.clone()), pending);

        let notify = SessionNotify::AcpRequestPermission(Box::new(AcpRequestPermission {
            session_id: session_id.clone(),
            request_permission_request: request_json,
            tool_call_id: tool_call_id.clone(),
            save_rule: save_rule_json,
            request_id: context.request_id.clone(),
        }));

        if let Err(err) =
            push_session_update_with_project(&context.project_id, &session_id, notify).await
        {
            error!(
                "[Permission] failed to push permission SSE event: project_id={}, session_id={}, error={}",
                context.project_id, session_id, err
            );
            let key = (session_id.clone(), tool_call_id);
            if let Some(pending) = self.pending.lock().remove(&key) {
                warn!(
                    "[Permission] SSE push failed, cancelling pending permission: session_id={}, tool_call_id={}",
                    key.0, key.1
                );
                let _ = pending.responder.respond(cancelled_response());
            }
        }

        Ok(())
    }
}

fn respond_with_preferred_option(
    request: &RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    preferred: &[PermissionOptionKind],
) -> Result<(), agent_client_protocol::Error> {
    let selected = select_option(&request.options, preferred).or_else(|| request.options.first());
    if let Some(option) = selected {
        responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
        ))
    } else {
        responder.respond(cancelled_response())
    }
}

fn select_option<'a>(
    options: &'a [PermissionOption],
    preferred: &[PermissionOptionKind],
) -> Option<&'a PermissionOption> {
    for kind in preferred {
        if let Some(option) = options.iter().find(|option| option.kind == *kind) {
            return Some(option);
        }
    }
    None
}

fn cancelled_response() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}

fn extract_tool_name(request: &RequestPermissionRequest) -> String {
    request
        .tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|value| value.get("tool_name").or_else(|| value.get("toolName")))
        .and_then(|value| value.as_str())
        .or_else(|| {
            request
                .tool_call
                .fields
                .title
                .as_deref()
                .map(|title| title.split_whitespace().next().unwrap_or("tool"))
        })
        .unwrap_or("tool")
        .to_string()
}

fn extract_command(request: &RequestPermissionRequest) -> Option<String> {
    request
        .tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|value| {
            value
                .get("command")
                .or_else(|| value.pointer("/input/command"))
        })
        .and_then(|value| value.as_str())
        .map(|s| s.to_string())
}

/// Hardcoded safety rules that always reject before any user-saved rule is consulted.
///
/// Priority chain (highest first):
/// 1. Dangerous-command rejection (this function) — cannot be overridden
/// 2. User deny rules (always_deny)
/// 3. User allow rules (always_allow)
/// 4. agent_mode fallback (yolo = auto-allow, ask = push SSE)
fn is_dangerous_command(command: Option<&str>) -> bool {
    let Some(command) = command else {
        return false;
    };

    // Strip `sudo` prefix and any sudo-specific flags (e.g. `sudo -E rm -rf /`).
    let command = strip_sudo_and_flags(command);

    // Split on chain operators to catch patterns like `rm -rf /tmp && rm -rf /`.
    for segment in split_commands(&command) {
        if is_single_command_dangerous(segment) {
            return true;
        }
    }

    false
}

/// Strip `sudo` and any flags that follow it until the actual command is reached.
fn strip_sudo_and_flags(command: &str) -> String {
    let rest = command.strip_prefix("sudo").map(str::trim).unwrap_or(command);
    let mut tokens = rest.split_whitespace();
    while let Some(token) = tokens.next() {
        if token.starts_with('-') {
            if let Some(flag_body) = token.strip_prefix("--") {
                // Long flags: `--user=root` (value attached) vs `--user root` (separate value).
                if !flag_body.contains('=') {
                    let _ = tokens.next(); // consume the value
                }
            } else {
                // Short flags: only consume a value for flags known to take one.
                // Sudo flags that take a value: -u, -g, -p, -h, -r, -t, -C.
                // Flags like -E, -n, -S, -s, -i, -b, -k, -K, -v, -V, -l, -A don't.
                if token.len() == 2 {
                    let takes_value =
                        matches!(token.as_bytes()[1], b'u' | b'g' | b'p' | b'h' | b'r' | b't' | b'C');
                    if takes_value {
                        let _ = tokens.next();
                    }
                }
                // Compound short flags like `-En` are all boolean — no value consumed.
            }
            continue;
        }
        // Reached the actual command — return the rest of the string.
        let remainder: Vec<&str> = std::iter::once(token).chain(tokens).collect();
        return remainder.join(" ");
    }
    String::new()
}

/// Split a command on chain operators (`&&`, `;`, `||`) so each segment is checked independently.
fn split_commands(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut current_start = 0;
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b';' {
            segments.push(command[current_start..i].trim());
            current_start = i + 1;
        } else if bytes[i] == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            segments.push(command[current_start..i].trim());
            current_start = i + 2;
            i += 1; // skip the second &
        } else if bytes[i] == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            segments.push(command[current_start..i].trim());
            current_start = i + 2;
            i += 1; // skip the second |
        }
        i += 1;
    }
    let last = command[current_start..].trim();
    if !last.is_empty() {
        segments.push(last);
    }
    if segments.is_empty() {
        vec![command]
    } else {
        segments
    }
}

/// Check a single command (no chain operators) for dangerous rm patterns.
fn is_single_command_dangerous(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();

    for (idx, token) in tokens.iter().enumerate() {
        if *token != "rm" {
            continue;
        }

        let mut recursive = false;
        let mut force = false;
        let mut saw_dash_dash = false;
        let mut targets: Vec<&str> = Vec::new();

        for token in tokens.iter().skip(idx + 1) {
            if *token == "--" {
                saw_dash_dash = true;
                continue;
            }

            if saw_dash_dash {
                targets.push(token);
                continue;
            }

            if let Some(flag_body) = token.strip_prefix("--") {
                if flag_body.is_empty() {
                    saw_dash_dash = true;
                    continue;
                }
                if let Some(name) = flag_body.split('=').next() {
                    match name {
                        "recursive" => recursive = true,
                        "force" => force = true,
                        _ => {}
                    }
                }
                continue;
            }

            if let Some(flags) = token.strip_prefix('-') {
                recursive |= flags.contains('r') || flags.contains('R');
                force |= flags.contains('f');
                continue;
            }

            targets.push(token);
        }

        if recursive && force {
            for target in &targets {
                if is_dangerous_rm_target(target) {
                    return true;
                }
            }
        }
    }

    false
}

/// Returns `true` when `token` is a globally destructive rm target.
fn is_dangerous_rm_target(token: &str) -> bool {
    // Root filesystem
    if token == "/" || token == "/*" {
        return true;
    }
    // Home directory (literal tilde)
    if token == "~" || token == "~/" || token == "~/*" {
        return true;
    }
    // $HOME / ${HOME}
    if token == "$HOME" || token == "${HOME}" || token == "$HOME/" || token == "${HOME}/" {
        return true;
    }
    if token == "$HOME/*" || token == "${HOME}/*" {
        return true;
    }
    // Current directory
    if token == "." || token == "./" || token == "./*" {
        return true;
    }
    // Parent directory
    if token == ".." || token == "../" || token == "../*" {
        return true;
    }
    // Path traversal (contains /../)
    if token.contains("/../") {
        return true;
    }
    false
}

fn build_save_rule_suggestion(
    tool_name: &str,
    command: Option<&str>,
) -> Option<SaveRuleSuggestion> {
    let command = command?.trim();
    let prefix = extract_terminal_command_prefix(command)?;
    Some(SaveRuleSuggestion {
        tool_name: tool_name.to_string(),
        pattern: terminal_pattern_from_tokens(&prefix.tokens)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandPrefix {
    tokens: Vec<String>,
}

fn extract_terminal_command_prefix(command: &str) -> Option<CommandPrefix> {
    let tokens = shlex::split(command)?;
    let mut normalized_tokens = Vec::new();
    let mut index = 0;

    while let Some(token) = tokens.get(index) {
        if is_assignment_token(token) {
            normalized_tokens.push(token.clone());
            index += 1;
        } else {
            break;
        }
    }

    let command_name = tokens.get(index)?.clone();
    if !is_plain_command_token(&command_name) {
        return None;
    }
    normalized_tokens.push(command_name);
    index += 1;

    while let Some(token) = tokens.get(index) {
        if is_redirect_token(token) {
            index += 1;
            continue;
        }
        if !token.starts_with('-') {
            if !is_plain_command_token(token) {
                return None;
            }
            normalized_tokens.push(token.clone());
        }
        break;
    }

    Some(CommandPrefix {
        tokens: normalized_tokens,
    })
}

fn terminal_pattern_from_tokens(tokens: &[String]) -> Option<String> {
    match tokens {
        [] => None,
        [single] => Some(format!("^{}\\b", escape_for_pattern(single))),
        [rest @ .., last] => Some(format!(
            "^{}\\s+{}(\\s|$)",
            rest.iter()
                .map(|token| escape_for_pattern(token))
                .collect::<Vec<_>>()
                .join("\\s+"),
            escape_for_pattern(last)
        )),
    }
}

fn is_assignment_token(token: &str) -> bool {
    let Some((name, value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !value.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_plain_command_token(token: &str) -> bool {
    !token.starts_with('-')
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn is_redirect_token(token: &str) -> bool {
    token.contains('>') || token.contains('<')
}

fn command_matches_pattern(command: &str, rule: &PermissionRule) -> bool {
    rule.compiled
        .as_ref()
        .map(|regex| regex.is_match(command))
        .unwrap_or(false)
}

fn escape_for_pattern(input: &str) -> String {
    regex::escape(input).replace("\\-", "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_rm_patterns_are_detected() {
        // Basic dangerous patterns
        assert!(is_dangerous_command(Some("rm -rf /")));
        assert!(is_dangerous_command(Some("sudo rm -rf $HOME")));
        assert!(is_dangerous_command(Some("rm   -rf   ~")));
        assert!(is_dangerous_command(Some("rm -fr ${HOME}")));
        // sudo prefix
        assert!(is_dangerous_command(Some("sudo rm -rf /")));
        // -- separator
        assert!(is_dangerous_command(Some("rm -rf -- /")));
        // long flags
        assert!(is_dangerous_command(Some("rm --recursive --force /")));
        assert!(is_dangerous_command(Some("rm --recursive=yes --force ~")));
        // path traversal
        assert!(is_dangerous_command(Some("rm -rf /tmp/../../")));
        // current / parent dir
        assert!(is_dangerous_command(Some("rm -rf .")));
        assert!(is_dangerous_command(Some("rm -rf ..")));
        assert!(is_dangerous_command(Some("rm -rf ./*")));
        // flag and target order independence
        assert!(is_dangerous_command(Some("rm / -rf")));
        assert!(is_dangerous_command(Some("rm / -r -f")));
        assert!(is_dangerous_command(Some("rm $HOME -rf")));
        // target after --
        assert!(is_dangerous_command(Some("rm -rf -- /")));
        // safe patterns
        assert!(!is_dangerous_command(Some("rm -rf target")));
        assert!(!is_dangerous_command(Some("rm -rf /tmp")));
        assert!(!is_dangerous_command(Some("rm file.txt")));
        assert!(!is_dangerous_command(Some("cargo build")));
        // `rm -- -rf /` → `-rf` is a file after `--`, not a flag; `rm /` fails on dir
        assert!(!is_dangerous_command(Some("rm -- -rf /")));
    }

    #[test]
    fn dangerous_sudo_with_flags_detected() {
        assert!(is_dangerous_command(Some("sudo -E rm -rf /")));
        assert!(is_dangerous_command(Some("sudo -n rm -rf ~")));
        assert!(is_dangerous_command(Some("sudo -u root rm -rf /")));
        assert!(is_dangerous_command(Some("sudo --user root rm -rf $HOME")));
        assert!(is_dangerous_command(Some("sudo -E -n rm -rf ../")));
        // safe sudo commands
        assert!(!is_dangerous_command(Some("sudo cargo build")));
        assert!(!is_dangerous_command(Some("sudo systemctl restart nginx")));
    }

    #[test]
    fn chained_dangerous_commands_detected() {
        assert!(is_dangerous_command(Some("rm -rf /tmp && rm -rf /")));
        assert!(is_dangerous_command(Some("echo hello ; rm -rf ~")));
        assert!(is_dangerous_command(Some("cargo build && rm -rf /")));
        assert!(is_dangerous_command(Some("make test || rm -rf $HOME")));
        // safe chained commands
        assert!(!is_dangerous_command(Some("cargo build && cargo test")));
        assert!(!is_dangerous_command(Some("git add . ; git commit -m msg")));
    }

    #[test]
    fn save_rule_suggestion_skips_script_paths() {
        assert!(build_save_rule_suggestion("bash", Some("cargo build")).is_some());
        assert!(build_save_rule_suggestion("bash", Some("./script.sh")).is_none());
        assert!(build_save_rule_suggestion("bash", Some("/bin/rm x")).is_none());
        // rm is a valid command token; pattern extraction should work.
        // The hardcoded dangerous-command rules reject truly dangerous rm invocations.
        assert!(build_save_rule_suggestion("bash", Some("rm -rf target")).is_some());
    }

    #[test]
    fn command_pattern_matches_simple_generated_rules() {
        let rule_allow_build = PermissionRule {
            decision: RuleDecision::Allow,
            pattern: "^cargo\\s+build(\\s|$)".to_string(),
            compiled: regex::Regex::new("^cargo\\s+build(\\s|$)").ok(),
        };
        assert!(command_matches_pattern("cargo build --release", &rule_allow_build));
        assert!(!command_matches_pattern("cargo test", &rule_allow_build));
    }

    // === rule_decision + save_rule_from_option_kind tests ===

    fn test_context(project_id: &str, user_id: &str) -> PermissionRequestContext {
        PermissionRequestContext {
            project_id: project_id.to_string(),
            user_id: if user_id.is_empty() {
                None
            } else {
                Some(user_id.to_string())
            },
            agent_mode: AgentMode::Ask,
            service_type: shared_types::ServiceType::RCoder,
            request_id: None,
        }
    }

    #[test]
    fn rule_decision_deny_beats_allow() {
        let pm = PermissionManager::default();
        let ctx = test_context("proj1", "user1");

        // Add an allow rule first
        pm.save_rule_from_option_kind(
            &ctx,
            &SaveRuleSuggestion {
                tool_name: "bash".to_string(),
                pattern: "^cargo\\s+.*".to_string(),
            },
            PermissionOptionKind::AllowAlways,
        );
        // Then add a deny rule targeting the same tool
        pm.save_rule_from_option_kind(
            &ctx,
            &SaveRuleSuggestion {
                tool_name: "bash".to_string(),
                pattern: "^cargo\\s+build".to_string(),
            },
            PermissionOptionKind::RejectAlways,
        );

        // Both patterns match "cargo build", deny must win
        assert_eq!(
            pm.rule_decision(&ctx, "bash", Some("cargo build")),
            Some(RuleDecision::Deny)
        );

        // Only allow pattern matches "cargo test"
        assert_eq!(
            pm.rule_decision(&ctx, "bash", Some("cargo test")),
            Some(RuleDecision::Allow)
        );

        // No pattern matches "npm install"
        assert_eq!(pm.rule_decision(&ctx, "bash", Some("npm install")), None);
    }

    #[test]
    fn rule_decision_falls_back_to_project_wide_rules() {
        let pm = PermissionManager::default();
        let user_ctx = test_context("proj1", "user1");
        let proj_ctx = test_context("proj1", ""); // anonymous project-wide

        // Add project-wide allow rule (no user_id)
        pm.save_rule_from_option_kind(
            &proj_ctx,
            &SaveRuleSuggestion {
                tool_name: "bash".to_string(),
                pattern: "^ls\\s+.*".to_string(),
            },
            PermissionOptionKind::AllowAlways,
        );

        // Any user in project "proj1" should match
        assert_eq!(
            pm.rule_decision(&user_ctx, "bash", Some("ls -la")),
            Some(RuleDecision::Allow)
        );

        // User from a different project should NOT match
        let other_ctx = test_context("proj2", "user2");
        assert_eq!(
            pm.rule_decision(&other_ctx, "bash", Some("ls -la")),
            None,
        );
    }

    #[test]
    fn save_rule_from_option_kind_only_stores_persistent_kinds() {
        let pm = PermissionManager::default();
        let ctx = test_context("proj1", "user1");
        let suggestion = SaveRuleSuggestion {
            tool_name: "bash".to_string(),
            pattern: "^npm\\s+.*".to_string(),
        };

        // AllowOnce should NOT persist a rule
        assert!(!pm.save_rule_from_option_kind(
            &ctx,
            &suggestion,
            PermissionOptionKind::AllowOnce
        ));

        // RejectOnce should NOT persist a rule
        assert!(!pm.save_rule_from_option_kind(
            &ctx,
            &suggestion,
            PermissionOptionKind::RejectOnce
        ));

        // AllowAlways should persist
        assert!(pm.save_rule_from_option_kind(
            &ctx,
            &suggestion,
            PermissionOptionKind::AllowAlways
        ));

        // RejectAlways should persist
        assert!(pm.save_rule_from_option_kind(
            &ctx,
            &suggestion,
            PermissionOptionKind::RejectAlways
        ));

        // Verify the stored rule is matched
        assert_eq!(
            pm.rule_decision(&ctx, "bash", Some("npm install")),
            Some(RuleDecision::Deny) // deny beats allow since RejectAlways was stored last
        );
    }
}

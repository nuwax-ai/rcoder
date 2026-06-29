use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use agent_abstraction::{PermissionRequestContext, PermissionRequestHandler};
use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::{Mutex, MutexGuard};
use shared_types::{
    AcpRequestPermission, AgentMode, ResolvePermissionRequestDto, ResolvePermissionResponseDto,
    SessionNotify, ToolApprovalAction,
};
use tracing::{error, info, warn};

use super::push_session_update_with_project;

pub static PERMISSION_MANAGER: LazyLock<Arc<PermissionManager>> =
    LazyLock::new(|| Arc::new(PermissionManager::default()));

type PendingKey = (String, String);
type RuleKey = (String, String, String);

struct PendingPermission {
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    context: PermissionRequestContext,
    save_rule: Option<SaveRuleSuggestion>,
}

/// 从权限请求中提取的字段，用于传递给 push_permission_to_frontend
struct ExtractedPermissionInfo {
    session_id: String,
    tool_call_id: String,
    tool_name: String,
    command: Option<String>,
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

/// per-session 动态权限状态（key = session_id）。
/// 复用 session 时，PermissionRequestContext 是旧 start_config 的快照（agent_mode/tool_approval_rules 已过期），
/// 通过此状态表覆盖，让每次请求携带的新值在 handler 决策时生效。
#[derive(Debug, Clone)]
struct SessionPermissionState {
    agent_mode: AgentMode,
    tool_approval_rules: Option<Vec<shared_types::ToolApprovalRule>>,
}

pub struct PermissionManager {
    pending: Mutex<HashMap<PendingKey, PendingPermission>>,
    rules: DashMap<RuleKey, Vec<PermissionRule>>,
    /// per-session 动态权限状态（key = session_id）。
    session_state: DashMap<String, SessionPermissionState>,
}

impl Default for PermissionManager {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            rules: DashMap::new(),
            session_state: DashMap::new(),
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

        info!(
            "[Permission] Resolving permission request: session_id={}, tool_call_id={}, cancelled={}, option_id={:?}",
            session_id, tool_call_id, input.cancelled, input.option_id
        );

        let Some(pending) = self.pending.lock().remove(&key) else {
            warn!(
                "[Permission] Permission request not found: session_id={}, tool_call_id={}",
                session_id, tool_call_id
            );
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
                        error_code: Some(shared_types::error_codes::ERR_VALIDATION.to_string()),
                        message: Some("option_id is required when cancelled is false".to_string()),
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
        if input.save_rule
            && let (Some(suggestion), Some(kind)) = (&pending.save_rule, selected_kind)
        {
            rule_saved = self.save_rule_from_option_kind(&pending.context, suggestion, kind);
        }

        let outcome_json = serde_json::to_string(&response).ok();
        match pending.responder.respond(response) {
            Ok(()) => {
                info!(
                    "[Permission] Permission resolved successfully: session_id={}, tool_call_id={}, rule_saved={}",
                    session_id, tool_call_id, rule_saved
                );
                ResolvePermissionResponseDto {
                    success: true,
                    session_id,
                    tool_call_id,
                    outcome_json,
                    rule_saved,
                    error_code: None,
                    message: None,
                }
            }
            Err(err) => {
                error!(
                    "[Permission] Failed to respond to permission request: session_id={}, tool_call_id={}, error={}",
                    session_id, tool_call_id, err
                );
                ResolvePermissionResponseDto {
                    success: false,
                    session_id,
                    tool_call_id,
                    outcome_json,
                    rule_saved,
                    error_code: Some(
                        shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED.to_string(),
                    ),
                    message: Some(err.to_string()),
                }
            }
        }
    }

    pub fn cancel_session_permissions(&self, session_id: &str) -> usize {
        info!(
            "[Permission] Cancelling all pending permissions for session: {}",
            session_id
        );
        let mut guard = self.pending.lock();
        let keys: Vec<_> = guard
            .keys()
            .filter(|key| key.0 == session_id)
            .cloned()
            .collect();
        let count = Self::remove_and_respond(&mut guard, &keys);
        drop(guard);
        info!(
            "[Permission] Cancelled {} pending permissions for session: {}",
            count, session_id
        );
        count
    }

    /// 注册/更新某 session 的动态权限状态（upsert）。
    /// 由 agent_session_service 在每次请求处理时调用（新 session 注册、复用 session 更新）。
    /// 复用 session 时 PermissionRequestContext 是旧 start_config 快照，需通过此表覆盖
    /// agent_mode/tool_approval_rules，让每次请求携带的新值在 handler 决策时生效。
    pub fn upsert_session_state(
        &self,
        session_id: &str,
        agent_mode: AgentMode,
        tool_approval_rules: Option<Vec<shared_types::ToolApprovalRule>>,
    ) {
        use dashmap::mapref::entry::Entry;
        let trimmed = session_id.trim();
        if trimmed.is_empty() {
            warn!("[Permission] upsert_session_state called with empty session_id, ignoring");
            return;
        }
        match self.session_state.entry(trimmed.to_string()) {
            Entry::Occupied(mut occ) => {
                let prev_mode = occ.get().agent_mode;
                occ.insert(SessionPermissionState {
                    agent_mode,
                    tool_approval_rules,
                });
                info!(
                    "[Permission] session_state updated: session_id={}, prev_mode={:?}, new_mode={:?}",
                    trimmed, prev_mode, agent_mode
                );
            }
            Entry::Vacant(vac) => {
                vac.insert(SessionPermissionState {
                    agent_mode,
                    tool_approval_rules,
                });
                info!(
                    "[Permission] session_state registered: session_id={}, agent_mode={:?}",
                    trimmed, agent_mode
                );
            }
        }
    }

    /// 清除某 session 的动态权限状态（session 结束/取消时调用，防止 DashMap 泄漏）。
    pub fn clear_session_state(&self, session_id: &str) {
        if let Some((_, removed)) = self.session_state.remove(session_id) {
            info!(
                "[Permission] session_state cleared: session_id={}, agent_mode={:?}",
                session_id, removed.agent_mode
            );
        }
    }

    /// 取 effective PermissionRequestContext：优先用 session_state 覆盖 agent_mode/tool_approval_rules，
    /// 未命中则返回原 context（fallback 旧行为）。返回值含来源标注（日志用）。
    fn effective_context_for(
        &self,
        session_id: &str,
        context: PermissionRequestContext,
    ) -> (PermissionRequestContext, &'static str) {
        match self.session_state.get(session_id) {
            Some(state) => {
                let mut c = context;
                c.agent_mode = state.agent_mode;
                c.tool_approval_rules = state.tool_approval_rules.clone();
                (c, "session_state")
            }
            None => (context, "context"),
        }
    }

    pub fn cancel_project_permissions(&self, project_id: &str) -> usize {
        info!(
            "[Permission] Cancelling all pending permissions for project: {}",
            project_id
        );
        let mut guard = self.pending.lock();
        let keys: Vec<_> = guard
            .iter()
            .filter(|(_, pending)| pending.context.project_id == project_id)
            .map(|(key, _)| key.clone())
            .collect();
        let count = Self::remove_and_respond(&mut guard, &keys);
        drop(guard);
        info!(
            "[Permission] Cancelled {} pending permissions for project: {}",
            count, project_id
        );
        count
    }

    fn remove_and_respond(
        guard: &mut MutexGuard<'_, HashMap<PendingKey, PendingPermission>>,
        keys: &[PendingKey],
    ) -> usize {
        let mut count = 0;
        for key in keys {
            if let Some(pending) = guard.remove(key) {
                if let Err(err) = pending.responder.respond(cancelled_response()) {
                    warn!("[Permission] failed to cancel pending permission: {err}");
                }
                count += 1;
            }
        }
        count
    }

    fn store_pending(&self, key: PendingKey, pending: PendingPermission) {
        let replaced = self.pending.lock().insert(key.clone(), pending);
        if let Some(replaced) = replaced {
            warn!(
                "[Permission] replaced duplicate pending permission, cancelling previous responder: session_id={}, tool_call_id={}",
                key.0, key.1
            );
            if let Err(err) = replaced.responder.respond(cancelled_response()) {
                warn!("[Permission] failed to cancel replaced pending permission: {err}");
            }
        }
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
        let compiled = match regex::Regex::new(&suggestion.pattern) {
            Ok(compiled) => compiled,
            Err(err) => {
                warn!(
                    "[Permission] skip invalid permission rule pattern: project_id={}, tool_name={}, pattern={}, error={}",
                    context.project_id, suggestion.tool_name, suggestion.pattern, err
                );
                return false;
            }
        };
        let mut rules = self.rules.entry(key).or_default();
        rules.push(PermissionRule {
            decision,
            pattern: suggestion.pattern.clone(),
            compiled: Some(compiled),
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

    /// 将权限请求推送到前端 SSE，等待用户审批
    async fn push_permission_to_frontend(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
        info: &ExtractedPermissionInfo,
    ) -> Result<(), agent_client_protocol::Error> {
        let save_rule = build_save_rule_suggestion(&info.tool_name, info.command.as_deref());
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
            save_rule,
        };
        self.store_pending(
            (info.session_id.clone(), info.tool_call_id.clone()),
            pending,
        );

        let notify = SessionNotify::AcpRequestPermission(Box::new(AcpRequestPermission {
            session_id: info.session_id.clone(),
            request_permission_request: request_json,
            tool_call_id: info.tool_call_id.clone(),
            save_rule: save_rule_json,
            request_id: context.request_id.clone(),
        }));

        if let Err(err) =
            push_session_update_with_project(&context.project_id, &info.session_id, notify).await
        {
            error!(
                "[Permission] failed to push permission SSE event: project_id={}, session_id={}, error={}",
                context.project_id, info.session_id, err
            );
            let key = (info.session_id.clone(), info.tool_call_id.clone());
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

#[async_trait]
impl PermissionRequestHandler for PermissionManager {
    async fn handle_permission_request(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let info = ExtractedPermissionInfo {
            session_id: request.session_id.to_string(),
            tool_call_id: request.tool_call.tool_call_id.to_string(),
            tool_name: extract_tool_name(&request),
            command: extract_command(&request),
        };

        // 优先用 per-session 动态状态覆盖 context 的固化值（复用 session 时 context 已过期）。
        let (effective_context, mode_source) =
            self.effective_context_for(&info.session_id, context);

        info!(
            "[Permission] Received permission request: session_id={}, tool_call_id={}, tool={}, command={:?}, agent_mode={:?}, source={}",
            info.session_id,
            info.tool_call_id,
            info.tool_name,
            info.command,
            effective_context.agent_mode,
            mode_source
        );

        // 危险命令仅记录日志（观测/审计）：不拦截、不强制审批、不 deny——
        // 审批完全由后续 rule_decision / tool_approval_rules / agent_mode 决定。
        if is_dangerous_command(info.command.as_deref()) {
            warn!(
                "[Permission] dangerous command detected (log only, 不拦截): session_id={}, tool_call_id={}, command={:?}",
                info.session_id, info.tool_call_id, info.command
            );
        }

        if let Some(decision) =
            self.rule_decision(&effective_context, &info.tool_name, info.command.as_deref())
        {
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

        // tool_approval_rules 匹配（首条命中即停）
        if let Some(action) = match_tool_approval_rules(&effective_context, &request) {
            info!(
                "[Permission] tool_approval_rules matched: session_id={}, tool_call_id={}, action={:?}",
                info.session_id, info.tool_call_id, action
            );
            match action {
                ToolApprovalAction::Allow => {
                    return respond_with_preferred_option(
                        &request,
                        responder,
                        &[
                            PermissionOptionKind::AllowAlways,
                            PermissionOptionKind::AllowOnce,
                        ],
                    );
                }
                ToolApprovalAction::Deny => {
                    return respond_with_preferred_option(
                        &request,
                        responder,
                        &[
                            PermissionOptionKind::RejectAlways,
                            PermissionOptionKind::RejectOnce,
                        ],
                    );
                }
                ToolApprovalAction::Ask => {
                    // 与 Ask 模式相同: 推 SSE 到前端
                    return self
                        .push_permission_to_frontend(effective_context, request, responder, &info)
                        .await;
                }
            }
        }

        if effective_context.agent_mode == AgentMode::Yolo {
            info!(
                "[Permission] Yolo mode, auto-approving: session_id={}, tool_call_id={}, tool={}",
                info.session_id, info.tool_call_id, info.tool_name
            );
            return respond_with_preferred_option(
                &request,
                responder,
                &[
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::AllowOnce,
                ],
            );
        }

        info!(
            "[Permission] Ask mode, pushing SSE to frontend: session_id={}, tool_call_id={}, tool={}",
            info.session_id, info.tool_call_id, info.tool_name
        );

        self.push_permission_to_frontend(effective_context, request, responder, &info)
            .await
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

/// 检查 tool_approval_rules 中是否有规则命中（首条命中即停）
///
/// 匹配语义（与前端客户端统一的「双路径」标准，详见 docs/tool-approval-rules-spec.md）：
/// - `tool_kind: None`（通用规则）→ 不按 kind 过滤，目标取【多字段任一命中】
///   （command/cmd/script/input.command + tool_name/toolName + title + title 首词，去重跳空）
/// - `tool_kind: Some(x)`（精确规则）→ 仅匹配 kind == x（大小写不敏感），目标取【单字段】
///   （命令类 kind → command 族首个非空；其他 → tool_name 族首个非空，兜底 "tool"）
/// - 多 patterns 之间 OR；多字段之间 OR；多 rules 顺序优先（首条命中即停）
fn match_tool_approval_rules(
    context: &PermissionRequestContext,
    request: &RequestPermissionRequest,
) -> Option<ToolApprovalAction> {
    let rules = context.tool_approval_rules.as_ref()?;
    // Use explicit match instead of Debug formatting to avoid depending on
    // #[non_exhaustive] enum's Debug representation, which may change across
    // agent-client-protocol-schema crate versions.
    let kind_str = request
        .tool_call
        .fields
        .kind
        .as_ref()
        .map(|k| match k {
            agent_client_protocol::schema::v1::ToolKind::Read => "Read",
            agent_client_protocol::schema::v1::ToolKind::Edit => "Edit",
            agent_client_protocol::schema::v1::ToolKind::Delete => "Delete",
            agent_client_protocol::schema::v1::ToolKind::Move => "Move",
            agent_client_protocol::schema::v1::ToolKind::Search => "Search",
            agent_client_protocol::schema::v1::ToolKind::Execute => "Execute",
            agent_client_protocol::schema::v1::ToolKind::Think => "Think",
            agent_client_protocol::schema::v1::ToolKind::Fetch => "Fetch",
            agent_client_protocol::schema::v1::ToolKind::SwitchMode => "SwitchMode",
            _ => "Other",
        })
        .unwrap_or("Other")
        .to_string();

    for rule in rules {
        // kind 过滤：tool_kind: None → 不过滤；Some(x) → 大小写不敏感匹配 kind_str
        let rule_kind = rule.tool_kind.as_deref();
        if let Some(rk) = rule_kind
            && !kind_str.eq_ignore_ascii_case(rk)
        {
            continue;
        }

        // 选匹配目标：通用规则 → 多字段；显式 tool_kind → 单字段
        let mut targets: Vec<String> = match rule_kind {
            None => extract_all_targets(request),
            Some(rk) => vec![extract_target_by_kind(request, rk)],
        };
        dedup_preserve_order(&mut targets);
        if targets.is_empty() {
            continue;
        }

        // 通配符匹配（大小写不敏感）：任一 pattern × 任一 target 命中即触发（OR）
        for pattern in &rule.patterns {
            let pat = pattern.trim();
            if pat.is_empty() {
                continue;
            }
            if targets.iter().any(|t| glob_match(pat, t)) {
                return Some(rule.action.clone());
            }
        }
    }
    None
}

/// 使用 glob 通配符匹配目标字符串（大小写不敏感）
fn glob_match(pattern: &str, target: &str) -> bool {
    let Ok(glob) = globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .build()
    else {
        return false;
    };
    glob.compile_matcher().is_match(target)
}

/// raw_input 中视为「命令内容」的字段 key（按优先级），兼容不同 ACP agent 的命名
const COMMAND_KEYS: &[&str] = &["command", "cmd", "script"];
/// raw_input 中视为「工具名」的字段 key（按优先级）。
/// `tool` 为 nuwaxcode MCP 工具实际使用的 key（日志验证）；
/// `tool_name`/`toolName` 为防御性兼容（部分 agent 可能使用，nuwaxcode 未观察到）。
const TOOL_NAME_KEYS: &[&str] = &["tool", "tool_name", "toolName"];

/// 命令类 kind 集合：显式 tool_kind 命中这些值时，匹配目标取 command 族。
/// `execute` 为 ACP 标准；`bash`/`terminal`/`shell`/`command` 兼容部分 agent 的自定义 kind 命名。
fn is_command_like_kind(kind_lower: &str) -> bool {
    matches!(
        kind_lower,
        "execute" | "bash" | "terminal" | "shell" | "command"
    )
}

/// 收集 raw_input 中所有命令类字段值：
/// `command`/`cmd`/`script` + 字符串 rawInput（整体视为命令）
fn extract_command_values(request: &RequestPermissionRequest) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(raw) = request.tool_call.fields.raw_input.as_ref() {
        // raw_input 本身为字符串时，整体视为命令
        if let Some(s) = raw.as_str() {
            push_nonempty(&mut values, s);
        }
        for key in COMMAND_KEYS {
            if let Some(s) = raw.get(*key).and_then(|v| v.as_str()) {
                push_nonempty(&mut values, s);
            }
        }
    }
    values
}

/// 收集所有工具名字段值：`tool_name`/`toolName` + `title` 首词
fn extract_tool_name_values(request: &RequestPermissionRequest) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(raw) = request.tool_call.fields.raw_input.as_ref() {
        for key in TOOL_NAME_KEYS {
            if let Some(s) = raw.get(*key).and_then(|v| v.as_str()) {
                push_nonempty(&mut values, s);
            }
        }
    }
    if let Some(title) = request.tool_call.fields.title.as_deref()
        && let Some(first) = title.split_whitespace().next()
    {
        push_nonempty(&mut values, first);
    }
    values
}

/// 通用规则（tool_kind=None）的多字段目标：command 族 + tool_name 族 + title 完整。
/// 身份类字段全纳入，鲁棒应对不同 ACP agent 上报结构差异（不赌信息放在哪个字段）。
fn extract_all_targets(request: &RequestPermissionRequest) -> Vec<String> {
    let mut targets = Vec::new();
    targets.extend(extract_command_values(request));
    targets.extend(extract_tool_name_values(request));
    if let Some(title) = request.tool_call.fields.title.as_deref() {
        push_nonempty(&mut targets, title);
    }
    targets
}

/// 显式 tool_kind 的单字段目标：
/// 命令类 kind → command 族首个非空；其他 → tool_name 族首个非空（兜底 "tool"）
fn extract_target_by_kind(request: &RequestPermissionRequest, rule_kind: &str) -> String {
    if is_command_like_kind(&rule_kind.to_ascii_lowercase()) {
        extract_command_values(request)
            .into_iter()
            .next()
            .unwrap_or_default()
    } else {
        extract_tool_name_values(request)
            .into_iter()
            .next()
            .unwrap_or_else(|| "tool".to_string())
    }
}

/// 提取工具名（单值，首个非空，兜底 "tool"）。
/// 供日志展示与权限上下文使用；规则匹配请用 `extract_tool_name_values`/`extract_all_targets`。
fn extract_tool_name(request: &RequestPermissionRequest) -> String {
    extract_tool_name_values(request)
        .into_iter()
        .next()
        .unwrap_or_else(|| "tool".to_string())
}

/// 提取命令内容（单值，首个非空）。
/// 供日志展示与危险命令检测使用；规则匹配请用 `extract_command_values`/`extract_all_targets`。
fn extract_command(request: &RequestPermissionRequest) -> Option<String> {
    extract_command_values(request).into_iter().next()
}

/// 将 trim 后非空的字符串加入 Vec
fn push_nonempty(vec: &mut Vec<String>, s: &str) {
    let trimmed = s.trim();
    if !trimmed.is_empty() {
        vec.push(trimmed.to_string());
    }
}

/// 去重，保留首次出现的顺序
fn dedup_preserve_order(vec: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    vec.retain(|s| seen.insert(s.clone()));
}

/// Hardcoded safety rules that always reject before any user-saved rule is consulted.
///
/// Priority chain (highest first):
/// 1. Dangerous-command rejection — cannot be overridden (forces frontend approval)
/// 2. User deny/allow rules via `rule_decision` (always_deny, always_allow)
/// 3. tool_approval_rules matching (first-match-wins)
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
    let rest = command
        .strip_prefix("sudo")
        .map(str::trim)
        .unwrap_or(command);
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
                    let takes_value = matches!(
                        token.as_bytes()[1],
                        b'u' | b'g' | b'p' | b'h' | b'r' | b't' | b'C'
                    );
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
        } else if bytes[i] == b'\n' {
            // 换行符也是 shell 命令分隔符，必须与 `;`、`&&`、`||` 同等对待
            // 否则 "echo hello\nrm -rf /" 会被当成单一命令段，绕过危险命令检测
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
        [single] => Some(format!("^{}\\b\\z", escape_for_pattern(single))),
        [rest @ .., last] => Some(format!(
            "^{}\\s+{}(\\s|$)\\z",
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
            pattern: "^cargo\\s+build(\\s|$)\\z".to_string(),
            compiled: regex::Regex::new("^cargo\\s+build(\\s|$)\\z").ok(),
        };
        assert!(command_matches_pattern("cargo build", &rule_allow_build));
        assert!(!command_matches_pattern(
            "cargo build --release",
            &rule_allow_build
        ));
        assert!(!command_matches_pattern("cargo test", &rule_allow_build));
    }

    #[test]
    fn terminal_pattern_blocks_overmatch_via_chained_command() {
        // 修复前 pattern `^cargo\s+build(\s|$)` 会让 `cargo build && rm -rf /` 命中
        // 修复后 pattern 末尾 \z 锚定,确保只匹配完整命令本身
        let pattern_cargo_build =
            terminal_pattern_from_tokens(&["cargo".to_string(), "build".to_string()])
                .expect("pattern should be generated for two tokens");

        let re = regex::Regex::new(&pattern_cargo_build).expect("pattern should compile");
        // 完整命令应匹配
        assert!(re.is_match("cargo build"));
        // 带 flag 的命令(新行为下不匹配,需要保存更具体的规则)
        assert!(!re.is_match("cargo build --release"));
        // 链式危险命令不应通过此 allow 规则
        assert!(!re.is_match("cargo build && rm -rf /"));
        assert!(!re.is_match("cargo build; rm -rf $HOME"));
    }

    #[test]
    fn terminal_pattern_single_token_is_end_anchored() {
        // 单 token pattern 也需要 \z 锚定,防止 `ls && rm -rf /` 之类误匹配
        let pattern = terminal_pattern_from_tokens(&["ls".to_string()])
            .expect("pattern should be generated for single token");
        let re = regex::Regex::new(&pattern).expect("pattern should compile");
        assert!(re.is_match("ls"));
        assert!(!re.is_match("ls -la"));
        assert!(!re.is_match("ls && rm -rf /"));
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
            service_type: shared_types::ServiceType::WebAgentRunner,
            request_id: None,
            tool_approval_rules: None,
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
        assert_eq!(pm.rule_decision(&other_ctx, "bash", Some("ls -la")), None,);
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
        assert!(!pm.save_rule_from_option_kind(&ctx, &suggestion, PermissionOptionKind::AllowOnce));

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

    #[test]
    fn save_rule_from_option_kind_rejects_invalid_regex() {
        let pm = PermissionManager::default();
        let ctx = test_context("proj1", "user1");
        let suggestion = SaveRuleSuggestion {
            tool_name: "bash".to_string(),
            pattern: "[".to_string(),
        };

        assert!(!pm.save_rule_from_option_kind(
            &ctx,
            &suggestion,
            PermissionOptionKind::AllowAlways
        ));
        assert_eq!(pm.rule_decision(&ctx, "bash", Some("anything")), None);
    }

    // === glob_match tests ===

    #[test]
    fn glob_match_basic_wildcards() {
        assert!(glob_match("rm -rf *", "rm -rf /tmp"));
        assert!(glob_match("rm -rf *", "rm -rf /tmp/cache"));
        assert!(!glob_match("rm -rf *", "rm -f /tmp"));
        assert!(!glob_match("rm -rf *", "rmdir /tmp"));

        assert!(glob_match("ls *", "ls -la"));
        assert!(!glob_match("ls *", "ls")); // "ls *" requires space + content
        assert!(!glob_match("ls *", "lsof"));

        assert!(glob_match("*delete*", "file_delete"));
        assert!(glob_match("*delete*", "delete_item"));
        assert!(!glob_match("*delete*", "remove"));

        assert!(glob_match("sudo *", "sudo rm -rf"));
        assert!(!glob_match("sudo *", "pseudo"));
    }

    #[test]
    fn glob_match_case_insensitive() {
        assert!(glob_match("RM *", "rm -rf /tmp"));
        assert!(glob_match("rm *", "RM -RF /tmp"));
        assert!(glob_match("*DELETE*", "file_delete"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("rm ?", "rm f"));
        assert!(glob_match("rm ?", "rm x"));
        assert!(!glob_match("rm ?", "rm ff"));
    }

    #[test]
    fn glob_match_character_class() {
        assert!(glob_match("[rc]m", "rm"));
        assert!(glob_match("[rc]m", "cm"));
        assert!(!glob_match("[rc]m", "dm"));
    }

    #[test]
    fn glob_match_invalid_pattern_returns_false() {
        assert!(!glob_match("[invalid", "test"));
    }

    #[test]
    fn glob_match_empty_pattern_returns_false() {
        // Empty patterns are skipped in match_tool_approval_rules,
        // but glob_match itself handles them gracefully
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "something"));
    }

    // === match_tool_approval_rules tests ===

    fn make_request_context_with_rules(
        rules: Option<Vec<shared_types::ToolApprovalRule>>,
    ) -> PermissionRequestContext {
        PermissionRequestContext {
            project_id: "proj1".to_string(),
            user_id: Some("user1".to_string()),
            agent_mode: AgentMode::Yolo,
            service_type: shared_types::ServiceType::WebAgentRunner,
            request_id: None,
            tool_approval_rules: rules,
        }
    }

    fn make_execute_request(command: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Execute)
            .title("bash")
            .raw_input(serde_json::json!({"command": command}));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    fn make_read_request(tool_name: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Read)
            .title(tool_name)
            .raw_input(serde_json::json!({"tool_name": tool_name}));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    fn make_other_request(tool_name: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        // 真实 MCP 工具的形态：kind=Other，工具名通过 title 传递；
        // raw_input 是工具自有参数（通常不含 tool_name 字段，由 extract_tool_name 回退到 title）
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Other)
            .title(tool_name)
            .raw_input(serde_json::json!({"arg": "sample"}));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    /// Execute 工具，命令放在指定的 raw_input key（command/cmd/script）
    fn make_execute_request_with_field(
        command_key: &str,
        command: &str,
    ) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let raw = serde_json::json!({ command_key: command });
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Execute)
            .title("bash")
            .raw_input(raw);
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    /// Execute 工具，自定义 command + title（用于多字段任一命中测试）
    fn make_execute_request_with_title(command: &str, title: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Execute)
            .title(title)
            .raw_input(serde_json::json!({ "command": command }));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    /// 只有 title，raw_input 无身份字段（测 title 兜底匹配）
    fn make_title_only_request(title: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Other)
            .title(title)
            .raw_input(serde_json::json!({ "some_arg": "value" }));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    /// raw_input 为字符串（整体视为命令）
    fn make_raw_string_request(raw: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Execute)
            .raw_input(serde_json::json!(raw));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    /// nuwaxcode MCP 工具真实形态：kind=Other，工具名在 rawInput.tool，title 为展示名
    fn make_mcp_tool_request(tool: &str, title: &str) -> RequestPermissionRequest {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields, ToolKind};
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Other)
            .title(title)
            .raw_input(serde_json::json!({ "tool": tool, "code": "sample" }));
        let tool_call = ToolCallUpdate::new("tc1", fields);
        RequestPermissionRequest::new("session1", tool_call, vec![])
    }

    #[test]
    fn tool_approval_rules_execute_matches_command() {
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["rm -rf *".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None, // 通用规则：不按 kind 过滤，此处匹配 Execute 工具的命令
        }]));

        let req = make_execute_request("rm -rf /tmp/cache");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );

        // Non-matching command
        let req = make_execute_request("ls -la");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_read_matches_tool_name() {
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*read*".to_string(), "*list*".to_string()],
            action: ToolApprovalAction::Allow,
            tool_kind: Some("Read".to_string()),
        }]));

        let req = make_read_request("mcp__server__read_items");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Allow)
        );

        let req = make_read_request("mcp__server__list_items");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Allow)
        );

        // Non-matching tool name
        let req = make_read_request("mcp__server__delete_item");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_kind_mismatch_skips() {
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: Some("Delete".to_string()),
        }]));

        // Execute request should not match Delete rule
        let req = make_execute_request("rm -rf /tmp");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_first_match_wins() {
        let ctx = make_request_context_with_rules(Some(vec![
            shared_types::ToolApprovalRule {
                patterns: vec!["rm *".to_string()],
                action: ToolApprovalAction::Ask,
                tool_kind: None,
            },
            shared_types::ToolApprovalRule {
                patterns: vec!["*".to_string()],
                action: ToolApprovalAction::Deny,
                tool_kind: None,
            },
        ]));

        let req = make_execute_request("rm -rf /tmp");
        // First rule matches with Ask
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_no_rules_returns_none() {
        let ctx = make_request_context_with_rules(None);
        let req = make_execute_request("rm -rf /tmp");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_empty_rules_returns_none() {
        let ctx = make_request_context_with_rules(Some(vec![]));
        let req = make_execute_request("rm -rf /tmp");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_deny_action() {
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["sudo *".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: None,
        }]));

        let req = make_execute_request("sudo rm -rf /");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Deny)
        );
    }

    #[test]
    fn tool_approval_rules_multiple_patterns_or_logic() {
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec![
                "rm -rf *".to_string(),
                "sudo *".to_string(),
                "chmod 777 *".to_string(),
            ],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));

        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("rm -rf /tmp")),
            Some(ToolApprovalAction::Ask)
        );
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("sudo apt install")),
            Some(ToolApprovalAction::Ask)
        );
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("chmod 777 /var")),
            Some(ToolApprovalAction::Ask)
        );
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("ls -la")),
            None
        );
    }

    // === MCP / Other 工具规则匹配（本次修复核心）===

    #[test]
    fn tool_approval_rules_other_matches_with_none_kind() {
        // 核心修复目标：MCP 工具(kind=Other) + tool_kind=None 的通用规则 → 命中
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["mcp__*".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));

        let req = make_other_request("mcp__github__create_issue");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_other_matches_explicit_other() {
        // MCP 工具 + 显式 tool_kind=Other → 命中
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["mcp__*".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: Some("Other".to_string()),
        }]));

        let req = make_other_request("mcp__github__create_issue");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_other_skips_explicit_execute() {
        // MCP 工具(kind=Other) + 显式 tool_kind=Execute → 不命中（精确匹配）
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: Some("Execute".to_string()),
        }]));

        let req = make_other_request("mcp__github__create_issue");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_other_skips_explicit_read() {
        // MCP 工具(kind=Other) + 显式 tool_kind=Read → 不命中（精确匹配）
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*".to_string()],
            action: ToolApprovalAction::Allow,
            tool_kind: Some("Read".to_string()),
        }]));

        let req = make_other_request("mcp__github__read_items");
        assert_eq!(match_tool_approval_rules(&ctx, &req), None);
    }

    #[test]
    fn tool_approval_rules_none_covers_both_execute_and_other() {
        // 一条 None 规则同时覆盖 bash 命令和 MCP 工具名
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["mcp__*".to_string(), "rm -rf *".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));

        // MCP 工具（Other）→ target=tool_name → 匹配 "mcp__*"
        let req_mcp = make_other_request("mcp__github__create_issue");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req_mcp),
            Some(ToolApprovalAction::Ask)
        );

        // bash 命令（Execute）→ target=command → 匹配 "rm -rf *"
        let req_bash = make_execute_request("rm -rf /tmp/cache");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req_bash),
            Some(ToolApprovalAction::Ask)
        );

        // target 选择正确性：MCP 工具名走 tool_name 分支，不会拿去和命令 pattern 比对
        // "rm_helper_tool" 既不匹配 "mcp__*" 也不匹配 "rm -rf *"（后者要求空格分隔）
        let req_mcp_rm = make_other_request("rm_helper_tool");
        assert_eq!(match_tool_approval_rules(&ctx, &req_mcp_rm), None);
    }

    #[test]
    fn tool_approval_rules_target_selection_isolated() {
        // 隔离验证 target 选择：pattern 只能命中对应分支的目标
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["sudo *".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: None,
        }]));

        // Execute 工具 → 走 command → "sudo apt install" 命中
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("sudo apt install")),
            Some(ToolApprovalAction::Deny)
        );

        // Other 工具 → 走 tool_name → "sudo_tool" 不匹配 "sudo *"（要求空格分隔）
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_other_request("sudo_tool")),
            None
        );
    }

    #[test]
    fn tool_approval_rules_first_match_wins_mixed() {
        // 首条命中即停：混合 kind 规则
        let ctx = make_request_context_with_rules(Some(vec![
            shared_types::ToolApprovalRule {
                patterns: vec!["mcp__*".to_string()],
                action: ToolApprovalAction::Ask,
                tool_kind: None,
            },
            shared_types::ToolApprovalRule {
                patterns: vec!["*".to_string()],
                action: ToolApprovalAction::Deny,
                tool_kind: None,
            },
        ]));

        let req = make_other_request("mcp__github__create_issue");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask) // 第 1 条命中，不进第 2 条
        );
    }

    // === 多字段统一标准测试（tool_kind=None 通用规则走多字段任一命中）===

    #[test]
    fn tool_approval_rules_command_key_aliases() {
        // 通用规则匹配命令的不同 key 变体（command/cmd/script）
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["rm *".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request("rm file")),
            Some(ToolApprovalAction::Ask)
        );
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request_with_field("cmd", "rm file")),
            Some(ToolApprovalAction::Ask)
        );
        assert_eq!(
            match_tool_approval_rules(&ctx, &make_execute_request_with_field("script", "rm file")),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_title_fallback() {
        // 无 command/tool_name 时，靠 title 兜底匹配
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*dangerous*".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));
        let req = make_title_only_request("some_dangerous_tool");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_multi_field_any_match() {
        // 多字段任一命中：pattern 命中 title 但不命中 command
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*secret_tool*".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: None,
        }]));
        // command="ls"（不匹配），title="secret_tool_name"（匹配）
        let req = make_execute_request_with_title("ls", "secret_tool_name");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Deny)
        );
    }

    #[test]
    fn tool_approval_rules_kind_case_insensitive() {
        // tool_kind 大小写不敏感：tool_kind="execute" 匹配 kind=Execute
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["rm *".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: Some("execute".to_string()), // 小写
        }]));
        let req = make_execute_request("rm file"); // kind=Execute
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_explicit_command_kind_reads_cmd() {
        // 显式命令类 tool_kind → 取 command 族（含 cmd/script 变体）
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["rm *".to_string()],
            action: ToolApprovalAction::Deny,
            tool_kind: Some("Execute".to_string()),
        }]));
        // 命令在 cmd key，显式 Execute 应取到（command 族首个非空）
        let req = make_execute_request_with_field("cmd", "rm file");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Deny)
        );
    }

    #[test]
    fn tool_approval_rules_raw_string_input() {
        // raw_input 为字符串时，整体视为命令
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["rm *".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));
        let req = make_raw_string_request("rm file");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
    }

    #[test]
    fn tool_approval_rules_mcp_tool_field_matched() {
        // nuwaxcode MCP 工具真实形态：工具名在 rawInput.tool（electron-dev.log 验证）
        let ctx = make_request_context_with_rules(Some(vec![shared_types::ToolApprovalRule {
            patterns: vec!["*get_stock_data".to_string()],
            action: ToolApprovalAction::Ask,
            tool_kind: None,
        }]));
        // 有 title 时，tool 与 title 都能命中
        let req = make_mcp_tool_request("get_stock_data", "A_get_stock_data");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req),
            Some(ToolApprovalAction::Ask)
        );
        // title 为空时，靠 rawInput.tool 仍能命中（验证 tool 字段独立有效）
        let req_no_title = make_mcp_tool_request("get_stock_data", "");
        assert_eq!(
            match_tool_approval_rules(&ctx, &req_no_title),
            Some(ToolApprovalAction::Ask)
        );
    }

    // === per-session 动态权限状态（复用 session 时动态切换 agent_mode/tool_approval_rules）===

    #[test]
    fn upsert_session_state_overrides_context_agent_mode() {
        // upsert 后，effective_context 应覆盖 context 的 agent_mode
        let pm = PermissionManager::default();
        pm.upsert_session_state("ses1", AgentMode::Yolo, None);
        let ctx = test_context("proj1", "user1"); // agent_mode = Ask
        let (effective, source) = pm.effective_context_for("ses1", ctx);
        assert_eq!(effective.agent_mode, AgentMode::Yolo);
        assert_eq!(source, "session_state");
    }

    #[test]
    fn upsert_session_state_overwrites_on_second_call() {
        // 二次 upsert 覆盖前值
        let pm = PermissionManager::default();
        pm.upsert_session_state("ses1", AgentMode::Ask, None);
        pm.upsert_session_state("ses1", AgentMode::Yolo, None);
        let ctx = test_context("proj1", "user1");
        let (effective, _) = pm.effective_context_for("ses1", ctx);
        assert_eq!(effective.agent_mode, AgentMode::Yolo);
    }

    #[test]
    fn clear_session_state_falls_back_to_context() {
        // clear 后回退到 context（旧行为）
        let pm = PermissionManager::default();
        pm.upsert_session_state("ses1", AgentMode::Yolo, None);
        pm.clear_session_state("ses1");
        let ctx = test_context("proj1", "user1"); // Ask
        let (effective, source) = pm.effective_context_for("ses1", ctx);
        assert_eq!(effective.agent_mode, AgentMode::Ask);
        assert_eq!(source, "context");
    }

    #[test]
    fn upsert_session_state_ignores_empty_session_id() {
        // 空 session_id 不写入（不 panic）
        let pm = PermissionManager::default();
        pm.upsert_session_state("   ", AgentMode::Yolo, None);
        let ctx = test_context("proj1", "user1");
        let (effective, source) = pm.effective_context_for("   ", ctx);
        assert_eq!(source, "context");
        assert_eq!(effective.agent_mode, AgentMode::Ask);
    }

    #[test]
    fn upsert_session_state_overrides_context_tool_approval_rules() {
        // upsert 后，effective_context 应覆盖 context 的 tool_approval_rules（不仅 agent_mode）
        let pm = PermissionManager::default();
        pm.upsert_session_state(
            "ses1",
            AgentMode::Yolo,
            Some(vec![shared_types::ToolApprovalRule {
                patterns: vec!["*get_stock_data".to_string()],
                action: ToolApprovalAction::Ask,
                tool_kind: None,
            }]),
        );
        let ctx = test_context("proj1", "user1"); // tool_approval_rules: None
        let (effective, source) = pm.effective_context_for("ses1", ctx);
        assert_eq!(source, "session_state");
        let rules = effective
            .tool_approval_rules
            .expect("rules should be overridden by session_state");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].patterns, vec!["*get_stock_data".to_string()]);
    }
}

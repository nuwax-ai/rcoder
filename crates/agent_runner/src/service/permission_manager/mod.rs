use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use agent_abstraction::{PermissionRequestContext, PermissionRequestHandler};
use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
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

// ---- 拆分出的纯函数子模块（详见各文件）----
mod approval_rules;
mod command_safety;
mod extractors;
mod pattern;
mod response;

use approval_rules::match_tool_approval_rules;
use command_safety::is_dangerous_command;
use extractors::{extract_command, extract_tool_name};
use pattern::{
    PermissionRule, RuleDecision, SaveRuleSuggestion, build_save_rule_suggestion,
    command_matches_pattern,
};
use response::{cancelled_response, respond_with_preferred_option};

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
    /// 最近审批决策：(session_id, tool_call_id) → option kind。
    /// 供同 tool_call_id 的后续 permission 自动跟随（一次调用统一审批）。不设 TTL：清理靠
    /// "新 chat 请求 cancel 上轮 session"（agent_session_service::process_request）与显式
    /// cancel/stop、lifecycle 结束，统一走 cancel_session_permissions / clear_session_state。
    recent_resolutions: DashMap<PendingKey, PermissionOptionKind>,
}

impl Default for PermissionManager {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            rules: DashMap::new(),
            session_state: DashMap::new(),
            recent_resolutions: DashMap::new(),
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
        // 先暂存 key/kind，等 respond 成功后再写入 recent（响应失败不应让后续 permission 跟随）
        let recent_key = (session_id.clone(), tool_call_id.clone());
        let recent_kind = selected_kind;

        match pending.responder.respond(response) {
            Ok(()) => {
                // 记录最近决策，供同 tool_call_id 的后续 permission 自动跟随（如 ext 审批后到达的 bash）。
                if let Some(kind) = recent_kind {
                    self.recent_resolutions.insert(recent_key, kind);
                }
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
        // 一并清理该 session 的最近审批记录（防内存泄漏）
        self.recent_resolutions
            .retain(|(sid, _), _| sid != session_id);
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
        // 一并清理该 session 的最近审批记录（防内存泄漏，与 cancel_session_permissions 对齐）
        self.recent_resolutions
            .retain(|(sid, _), _| sid != session_id);
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

        // 同 tool_call_id 的后续 permission 自动跟随用户最近决策（方案 A：一次操作统一审批）。
        // nuwaxcode 对一次调用先后发多个 permission（external_directory + bash），前端按
        // tool_call_id 只弹一个框、用户只审批一次。此处命中 recent（用户已审批过该 tool_call_id）
        // 则自动套用同一决策，不再 push 前端、不进 pending，让 agent 立即继续。
        // 位置在 rule_decision / tool_approval_rules 之后：用户显式配置的 deny/allow 规则优先，
        // 规则未命中才走"同 tool_call_id 跟随"，避免规则被跟随绕过。
        // 不设 TTL：recent 在"新 chat 请求"/显式 cancel/lifecycle 结束时统一清理。
        {
            let key = (info.session_id.clone(), info.tool_call_id.clone());
            if let Some(kind) = self.recent_resolutions.view(&key, |_, v| *v) {
                info!(
                    "[Permission] auto-follow recent resolution: session_id={}, tool_call_id={}, tool={}, kind={:?}",
                    info.session_id, info.tool_call_id, info.tool_name, kind
                );
                return respond_with_preferred_option(&request, responder, &[kind]);
            }
        }

        if effective_context.agent_mode == AgentMode::Yolo {
            info!(
                "[Permission] Yolo mode, auto-approving: session_id={}, tool_call_id={}, tool={}",
                info.session_id, info.tool_call_id, info.tool_name
            );
            // 用 AllowOnce 而非 AllowAlways：AllowOnce 仅本次放行，不污染 nuwaxcode 的权限记忆。
            // 若首选 AllowAlways，nuwaxcode 会把它当作"永久放行"持久化，之后即便把 agent_mode 切回
            // Ask，nuwaxcode 也不再发起 permission request → rcoder 收不到 → 审批 SSE 无法触发
            // （即 "ask→yolo→ask 第三次不弹审批" 的根因）。AllowOnce 保证 yolo 仅"每次自动批"，
            // 让 agent_mode 切换始终可逆。
            return respond_with_preferred_option(
                &request,
                responder,
                &[PermissionOptionKind::AllowOnce],
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

#[cfg(test)]
mod tests;

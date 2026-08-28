//! 权限决策链（resolve_permission 主流程 + 规则判定/持久化 + pending 暂存）。
//!
//! 自 mod.rs 拆出；共享类型（PendingPermission / SessionPermissionState 等）与
//! 辅助方法经 `super::` 消费（pub(super) 可见性）。

use super::*;

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

        // 持 pending 锁一次性完成 remove + 全部同步校验 + 失败原地放回;成功则取出
        // (pending, response) 释放锁后再 consume。消除原来 remove 与 4 处 re-insert 之间的
        // 窗口里并发 store_pending 被覆盖、孤儿 Responder 的丢更新。
        let (pending, response) = {
            let mut guard = self.pending.lock();
            let Some(pending) = guard.remove(&key) else {
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
                    error_code: Some(
                        shared_types::error_codes::ERR_PERMISSION_NOT_FOUND.to_string(),
                    ),
                    message: Some("permission request not found or already resolved".to_string()),
                };
            };

            if let Some(project_id) = input.project_id.as_deref().filter(|s| !s.trim().is_empty())
                && project_id != pending.context.project_id
            {
                guard.insert(key, pending);
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
                guard.insert(key, pending);
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
                            guard.insert(key, pending);
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
                        guard.insert(key, pending);
                        return ResolvePermissionResponseDto {
                            success: false,
                            session_id,
                            tool_call_id,
                            outcome_json: None,
                            rule_saved: false,
                            error_code: Some(shared_types::error_codes::ERR_VALIDATION.to_string()),
                            message: Some(
                                "option_id is required when cancelled is false".to_string(),
                            ),
                        };
                    }
                }
            };
            (pending, response)
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

    /// 取 effective PermissionRequestContext：优先用 session_state 覆盖 agent_mode/tool_approval_rules，
    /// 未命中则返回原 context（fallback 旧行为）。返回值含来源标注（日志用）。
    pub(super) fn effective_context_for(
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

    pub(super) fn store_pending(&self, key: PendingKey, pending: PendingPermission) {
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

    pub(super) fn save_rule_from_option_kind(
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

    pub(super) fn rule_decision(
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

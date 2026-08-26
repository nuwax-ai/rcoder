use super::approval_rules::*;
use super::pattern::*;
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
    assert!(!pm.save_rule_from_option_kind(&ctx, &suggestion, PermissionOptionKind::RejectOnce));

    // AllowAlways should persist
    assert!(pm.save_rule_from_option_kind(&ctx, &suggestion, PermissionOptionKind::AllowAlways));

    // RejectAlways should persist
    assert!(pm.save_rule_from_option_kind(&ctx, &suggestion, PermissionOptionKind::RejectAlways));

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

    assert!(!pm.save_rule_from_option_kind(&ctx, &suggestion, PermissionOptionKind::AllowAlways));
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
fn make_execute_request_with_field(command_key: &str, command: &str) -> RequestPermissionRequest {
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

use agent_abstraction::PermissionRequestContext;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use shared_types::ToolApprovalAction;

use super::extractors::{dedup_preserve_order, extract_all_targets, extract_target_by_kind};

/// 检查 tool_approval_rules 中是否有规则命中（首条命中即停）
///
/// 匹配语义（与前端客户端统一的「双路径」标准，详见 docs/tool-approval-rules-spec.md）：
/// - `tool_kind: None`（通用规则）→ 不按 kind 过滤，目标取【多字段任一命中】
///   （command/cmd/script/input.command + tool_name/toolName + title + title 首词，去重跳空）
/// - `tool_kind: Some(x)`（精确规则）→ 仅匹配 kind == x（大小写不敏感），目标取【单字段】
///   （命令类 kind → command 族首个非空；其他 → tool_name 族首个非空，兜底 "tool"）
/// - 多 patterns 之间 OR；多字段之间 OR；多 rules 顺序优先（首条命中即停）
pub(super) fn match_tool_approval_rules(
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
pub(super) fn glob_match(pattern: &str, target: &str) -> bool {
    let Ok(glob) = globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .build()
    else {
        return false;
    };
    glob.compile_matcher().is_match(target)
}

use agent_client_protocol::schema::v1::RequestPermissionRequest;

/// raw_input 中视为「命令内容」的字段 key（按优先级），兼容不同 ACP agent 的命名
pub(super) const COMMAND_KEYS: &[&str] = &["command", "cmd", "script"];
/// raw_input 中视为「工具名」的字段 key（按优先级）。
/// `tool` 为 nuwaxcode MCP 工具实际使用的 key（日志验证）；
/// `tool_name`/`toolName` 为防御性兼容（部分 agent 可能使用，nuwaxcode 未观察到）。
pub(super) const TOOL_NAME_KEYS: &[&str] = &["tool", "tool_name", "toolName"];

/// 命令类 kind 集合：显式 tool_kind 命中这些值时，匹配目标取 command 族。
/// `execute` 为 ACP 标准；`bash`/`terminal`/`shell`/`command` 兼容部分 agent 的自定义 kind 命名。
pub(super) fn is_command_like_kind(kind_lower: &str) -> bool {
    matches!(
        kind_lower,
        "execute" | "bash" | "terminal" | "shell" | "command"
    )
}

/// 收集 raw_input 中所有命令类字段值：
/// `command`/`cmd`/`script` + 字符串 rawInput（整体视为命令）
pub(super) fn extract_command_values(request: &RequestPermissionRequest) -> Vec<String> {
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
pub(super) fn extract_tool_name_values(request: &RequestPermissionRequest) -> Vec<String> {
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
pub(super) fn extract_all_targets(request: &RequestPermissionRequest) -> Vec<String> {
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
pub(super) fn extract_target_by_kind(
    request: &RequestPermissionRequest,
    rule_kind: &str,
) -> String {
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
pub(super) fn extract_tool_name(request: &RequestPermissionRequest) -> String {
    extract_tool_name_values(request)
        .into_iter()
        .next()
        .unwrap_or_else(|| "tool".to_string())
}

/// 提取命令内容（单值，首个非空）。
/// 供日志展示与危险命令检测使用；规则匹配请用 `extract_command_values`/`extract_all_targets`。
pub(super) fn extract_command(request: &RequestPermissionRequest) -> Option<String> {
    extract_command_values(request).into_iter().next()
}

/// 将 trim 后非空的字符串加入 Vec
pub(super) fn push_nonempty(vec: &mut Vec<String>, s: &str) {
    let trimmed = s.trim();
    if !trimmed.is_empty() {
        vec.push(trimmed.to_string());
    }
}

/// 去重，保留首次出现的顺序
pub(super) fn dedup_preserve_order(vec: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    vec.retain(|s| seen.insert(s.clone()));
}

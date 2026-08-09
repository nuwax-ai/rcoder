#[derive(Debug, Clone)]
pub(super) struct SaveRuleSuggestion {
    pub(super) tool_name: String,
    pub(super) pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RuleDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub(super) struct PermissionRule {
    pub(super) decision: RuleDecision,
    /// Stored for debugging/inspection; the active engine is `compiled`.
    #[allow(dead_code)]
    pub(super) pattern: String,
    /// Compiled regex, created once at insertion time.
    pub(super) compiled: Option<regex::Regex>,
}

pub(super) fn build_save_rule_suggestion(
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
pub(super) struct CommandPrefix {
    tokens: Vec<String>,
}

pub(super) fn extract_terminal_command_prefix(command: &str) -> Option<CommandPrefix> {
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

pub(super) fn terminal_pattern_from_tokens(tokens: &[String]) -> Option<String> {
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

pub(super) fn is_assignment_token(token: &str) -> bool {
    let Some((name, value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !value.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub(super) fn is_plain_command_token(token: &str) -> bool {
    !token.starts_with('-')
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

pub(super) fn is_redirect_token(token: &str) -> bool {
    token.contains('>') || token.contains('<')
}

pub(super) fn command_matches_pattern(command: &str, rule: &PermissionRule) -> bool {
    rule.compiled
        .as_ref()
        .map(|regex| regex.is_match(command))
        .unwrap_or(false)
}

pub(super) fn escape_for_pattern(input: &str) -> String {
    regex::escape(input).replace("\\-", "-")
}

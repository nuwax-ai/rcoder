/// Hardcoded safety rules that always reject before any user-saved rule is consulted.
///
/// Priority chain (highest first):
/// 1. Dangerous-command rejection — cannot be overridden (forces frontend approval)
/// 2. User deny/allow rules via `rule_decision` (always_deny, always_allow)
/// 3. tool_approval_rules matching (first-match-wins)
/// 4. agent_mode fallback (yolo = auto-allow, ask = push SSE)
pub(super) fn is_dangerous_command(command: Option<&str>) -> bool {
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
pub(super) fn strip_sudo_and_flags(command: &str) -> String {
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
pub(super) fn split_commands(command: &str) -> Vec<&str> {
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
pub(super) fn is_single_command_dangerous(command: &str) -> bool {
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
pub(super) fn is_dangerous_rm_target(token: &str) -> bool {
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

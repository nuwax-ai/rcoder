//! hooks 配置解析 (对齐 nuwax `parseHooksConfigWithStatus` / `normalizeHooksMap` /
//! `parseHookHandlerObject`)。
//!
//! 把 hooksConfig JSON 字符串解析为规范化的 hooks map (事件 → matcher 分组 → handler 对象数组),
//! handler 若为 JSON 字符串则解析为对象。供 mod 主入口判断是否更新 hooks。

use serde_json::{Map, Value};

/// hooks 配置解析状态 (对齐 nuwax { attempted, hooksMap, error })。
pub(super) struct HooksStatus {
    pub attempted: bool,
    pub hooks_map: Option<Map<String, Value>>,
    pub error: Option<String>,
}

/// 解析 hooksConfig 字符串为规范化的 hooks map (对齐 nuwax parseHooksConfigWithStatus)。
/// 空输入 → 未尝试 (attempted=false); 解析失败 → attempted=true + error; 成功 → 规范化 hooksMap。
pub(super) fn parse_hooks_config_with_status(hooks_config: Option<&str>) -> HooksStatus {
    let Some(s) = hooks_config else {
        return HooksStatus {
            attempted: false,
            hooks_map: None,
            error: None,
        };
    };
    if s.trim().is_empty() {
        return HooksStatus {
            attempted: false,
            hooks_map: None,
            error: None,
        };
    }
    match serde_json::from_str::<Value>(s) {
        Ok(parsed) => HooksStatus {
            attempted: true,
            hooks_map: normalize_hooks_map(&parsed),
            error: None,
        },
        Err(e) => HooksStatus {
            attempted: true,
            hooks_map: None,
            error: Some(e.to_string()),
        },
    }
}

/// 规范化 hooks 配置: 确保 hooks 数组内为对象而非 JSON 字符串 (对齐 nuwax normalizeHooksMap)。
/// 非 object 输入 → None; 否则逐事件/分组/handler 规范化, 空结果 → None。
fn normalize_hooks_map(hooks_map: &Value) -> Option<Map<String, Value>> {
    let obj = hooks_map.as_object()?;
    let mut normalized = Map::new();
    for (event_name, matcher_groups) in obj {
        let Some(groups_arr) = matcher_groups.as_array() else {
            continue;
        };
        let mut groups: Vec<Value> = Vec::new();
        for group in groups_arr {
            let Some(group_obj) = group.as_object() else {
                continue;
            };
            let raw_handlers = group_obj.get("hooks").and_then(|h| h.as_array());
            let mut handlers: Vec<Value> = Vec::new();
            if let Some(raw_arr) = raw_handlers {
                for raw_handler in raw_arr {
                    if let Some(handler) = parse_hook_handler_object(raw_handler) {
                        handlers.push(handler);
                    }
                }
            }
            if handlers.is_empty() {
                continue;
            }
            let mut new_group = group_obj.clone();
            new_group.insert("hooks".to_string(), Value::Array(handlers));
            groups.push(Value::Object(new_group));
        }
        if !groups.is_empty() {
            normalized.insert(event_name.clone(), Value::Array(groups));
        }
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// 把单个 handler 规范化为 object (对齐 nuwax parseHookHandlerObject):
/// - 已是 object → 原样返回
/// - 是 JSON 字符串 → 解析, 解析为 object 则返回, 否则 None
/// - null / 数字 / 布尔 / 数组 → None
fn parse_hook_handler_object(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) if parsed.is_object() => Some(parsed),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hooks_status_empty() {
        let s = parse_hooks_config_with_status(None);
        assert!(!s.attempted);
        let s = parse_hooks_config_with_status(Some("   "));
        assert!(!s.attempted);
    }

    #[test]
    fn parse_hooks_status_invalid_records_error() {
        let s = parse_hooks_config_with_status(Some("{not json"));
        assert!(s.attempted);
        assert!(s.error.is_some());
        assert!(s.hooks_map.is_none());
    }

    #[test]
    fn parse_hooks_status_valid_normalizes() {
        let input =
            r#"{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"echo hi"}]}]}"#;
        let s = parse_hooks_config_with_status(Some(input));
        assert!(s.attempted);
        assert!(s.error.is_none());
        let hm = s.hooks_map.expect("hooks map");
        assert!(hm.contains_key("PreToolUse"));
    }

    #[test]
    fn normalize_hooks_map_drops_empty_groups() {
        let v: Value =
            serde_json::from_str(r#"{"PreToolUse":[{"matcher":"*","hooks":[]}]}"#).unwrap();
        assert!(normalize_hooks_map(&v).is_none());
    }

    #[test]
    fn normalize_hooks_map_parses_string_handler() {
        let v: Value = serde_json::from_str(
            r#"{"PreToolUse":[{"hooks":["{\"type\":\"command\",\"command\":\"x\"}"]}]}"#,
        )
        .unwrap();
        let hm = normalize_hooks_map(&v).expect("some");
        let groups = hm.get("PreToolUse").unwrap().as_array().unwrap();
        let handlers = groups[0]
            .as_object()
            .unwrap()
            .get("hooks")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].get("type").and_then(|v| v.as_str()),
            Some("command")
        );
    }
}

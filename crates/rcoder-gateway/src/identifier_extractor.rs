//! 从请求中提取路由标识符（user_id 或 project_id）
//!
//! 支持三种提取方式：
//! 1. 从 JSON body 字段提取
//! 2. 从 URL path 参数提取
//! 3. 通过 session_id 解析（由 SessionResolver 处理）

/// 标识符提取器
pub struct IdentifierExtractor;

impl IdentifierExtractor {
    /// 从 JSON body 提取指定字段
    ///
    /// body 格式示例：`{"user_id": "123", "prompt": "hello"}`
    /// 调用 `from_body(body, "user_id")` 返回 `Some("123")`
    pub fn from_body(body: &[u8], field: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let v = value.get(field)?;
        match v {
            serde_json::Value::String(s) => {
                if s.is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            }
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// 从 URL path 提取参数
    ///
    /// 例如 path = "/agent/status/user-123"，需要提取第 3 段
    /// 调用 `from_path(path, 2)` 返回 `Some("user-123")`
    pub fn from_path_by_index(path: &str, segment_index: usize) -> Option<String> {
        let segment = path.split('/').nth(segment_index)?;
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_body_string() {
        let body = br#"{"user_id": "user-123", "prompt": "hello"}"#;
        assert_eq!(
            IdentifierExtractor::from_body(body, "user_id"),
            Some("user-123".to_string())
        );
    }

    #[test]
    fn test_from_body_number() {
        let body = br#"{"project_id": 42}"#;
        assert_eq!(
            IdentifierExtractor::from_body(body, "project_id"),
            Some("42".to_string())
        );
    }

    #[test]
    fn test_from_body_missing_field() {
        let body = br#"{"other": "value"}"#;
        assert_eq!(IdentifierExtractor::from_body(body, "user_id"), None);
    }

    #[test]
    fn test_from_body_empty_string() {
        let body = br#"{"user_id": ""}"#;
        assert_eq!(IdentifierExtractor::from_body(body, "user_id"), None);
    }

    #[test]
    fn test_from_body_invalid_json() {
        let body = b"not json";
        assert_eq!(IdentifierExtractor::from_body(body, "user_id"), None);
    }

    #[test]
    fn test_from_path_by_index() {
        assert_eq!(
            IdentifierExtractor::from_path_by_index("/agent/status/user-123", 3),
            Some("user-123".to_string())
        );
    }

    #[test]
    fn test_from_path_out_of_bounds() {
        assert_eq!(IdentifierExtractor::from_path_by_index("/agent/status", 5), None);
    }

    #[test]
    fn test_from_path_empty_segment() {
        assert_eq!(IdentifierExtractor::from_path_by_index("/agent//status", 2), None);
    }
}

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// 灵活的字符串反序列化器
///
/// 支持将 JSON 字符串、数字、布尔值转换为 String 类型。
/// 用于兼容不同客户端（如 Java Long 类型 vs String 类型）。
///
/// # 支持的格式
/// - JSON 字符串：`"tenant_id": "123"` → `Some("123")`
/// - JSON 数字：`"tenant_id": 123` → `Some("123")`
/// - JSON null 或缺失：`null` 或不提供 → `None`
///
/// # 示例
/// ```rust
/// use serde::Deserialize;
/// use shared_types::flexible_string;
///
/// #[derive(Deserialize)]
/// struct MyRequest {
///     #[serde(default, deserialize_with = "flexible_string::flexible_string")]
///     pub tenant_id: Option<String>,
/// }
/// ```
pub fn flexible_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(other) => Ok(Some(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct TestRequest {
        #[serde(default, deserialize_with = "flexible_string")]
        pub value: Option<String>,
    }

    #[test]
    fn test_string_value() {
        let json = r#"{"value": "123"}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, Some("123".to_string()));
    }

    #[test]
    fn test_number_value() {
        let json = r#"{"value": 123}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, Some("123".to_string()));
    }

    #[test]
    fn test_float_value() {
        let json = r#"{"value": 123.456}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, Some("123.456".to_string()));
    }

    #[test]
    fn test_null_value() {
        let json = r#"{"value": null}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, None);
    }

    #[test]
    fn test_missing_value() {
        let json = r#"{}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, None);
    }

    #[test]
    fn test_bool_value() {
        let json = r#"{"value": true}"#;
        let result: TestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(result.value, Some("true".to_string()));
    }
}

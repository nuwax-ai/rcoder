//! 共享 garde 校验规则（rcoder 侧；命名与用法对齐 file-server/validation_rules.rs）。
//!
//! handler 层 DTO 经 `#[derive(garde::Validate)]` + `#[garde(custom(...))]`
//! 声明式引用，取代 handler 内手动的 `validate_identifier(...)` 调用。
//! 规则本体复用 `shared_types::validate_identifier`（[A-Za-z0-9_-]、≤64、
//! 拒绝 `.`/`/` 防路径穿越——进容器名/PVC 名/subPath 的标识符约束）。

use garde::Error;

/// `String` 标识符：非空 + `shared_types::validate_identifier` 全规则。
pub fn identifier(value: &str, _: &()) -> garde::Result {
    shared_types::validate_identifier(value, "identifier").map_err(Error::new)
}

/// `Option<String>` 可选标识符：None / 纯空白跳过（可选语义），非空才校验
/// 格式。garde 的 `custom` 不自动解包 `Option`（同 file-server 惯例），
/// 单函数处理。
pub fn optional_identifier(value: &Option<String>, _: &()) -> garde::Result {
    match value.as_deref() {
        None => Ok(()),
        Some(v) if v.trim().is_empty() => Ok(()),
        Some(v) => shared_types::validate_identifier(v, "identifier").map_err(Error::new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_msg(result: garde::Result) -> String {
        result.err().map(|e| e.to_string()).unwrap_or_default()
    }

    #[test]
    fn identifier_accepts_valid() {
        assert!(identifier("app-Order_01", &()).is_ok());
    }

    #[test]
    fn identifier_rejects_dot_and_too_long() {
        assert!(
            identifier("a.b", &()).is_err(),
            "path traversal chars rejected"
        );
        let long = "a".repeat(65);
        assert!(identifier(&long, &()).is_err(), ">64 chars rejected");
        assert!(!err_msg(identifier(&long, &())).is_empty());
    }

    #[test]
    fn optional_identifier_skips_empty_but_validates_value() {
        assert!(optional_identifier(&None, &()).is_ok());
        assert!(optional_identifier(&Some("   ".into()), &()).is_ok());
        assert!(optional_identifier(&Some("u-123".into()), &()).is_ok());
        assert!(optional_identifier(&Some("bad/id".into()), &()).is_err());
    }
}

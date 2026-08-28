//! Validation utilities for converting garde errors to AppError

use crate::AppError;
use garde::Report;

/// 将 Garde Report 转换为 AppError
///
/// # Example
/// ```ignore
/// request.validate().map_err(garde_err_to_app_error)?;
/// ```
pub fn garde_err_to_app_error(report: Report) -> AppError {
    let errors: Vec<String> = report
        .iter()
        .map(|(path, err)| format!("{}: {}", path, err.message()))
        .collect();
    let message = errors.join("; ");
    AppError::validation_error(&message)
}

/// 校验路径标识符（project_id, agent_work_dir 等）
///
/// # 规则
/// - 仅允许 `[a-zA-Z0-9_-]`
/// - 长度 1-64 字符
/// - 不允许 `.`（防止路径穿越）
/// - 不允许 `/`（防止路径注入）
///
/// # 错误
/// 返回描述校验失败原因的字符串
pub fn validate_identifier(value: &str, field_name: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} 不能为空", field_name));
    }
    if value.len() > 64 {
        return Err(format!("{} 长度超过 64 字符: {}", field_name, value.len()));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "{} 包含非法字符: '{}'，仅允许字母、数字、下划线和连字符",
            field_name, value
        ));
    }
    Ok(())
}

/// 标识符白名单正则（garde 内置 pattern 规则用；DTO 字段
/// `#[garde(pattern(shared_types::IDENTIFIER_RE))]` 声明式标记）。
///
/// 语义与 [`validate_identifier`] 一致：字母数字下划线连字符、1-64 字符、
/// 防路径穿越/注入。`\A...\z` 严格锚定（`^...$` 在 Rust regex 默认允许尾部
/// 换行，白名单外的 `\n` 会漏过）。
pub static IDENTIFIER_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\A[a-zA-Z0-9_-]{1,64}\z").expect("identifier whitelist regex")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_regex_matches_validate_identifier_semantics() {
        // 同语义：合法集
        for ok in ["user123", "my-project_01", "A-B_C", "a", "12345"] {
            assert!(IDENTIFIER_RE.is_match(ok), "{ok}");
            assert!(validate_identifier(ok, "f").is_ok());
        }
        // 拒绝集：穿越/注入/超长/尾部换行（$ 锚定漏洞回归锚）
        let too_long = "x".repeat(65);
        for bad in ["../etc", "foo/bar", "", "a b", "abc\n", too_long.as_str()] {
            assert!(!IDENTIFIER_RE.is_match(bad), "{bad:?}");
        }
    }

    #[test]
    fn test_validate_identifier_accepts_valid() {
        assert!(validate_identifier("user123", "user_id").is_ok());
        assert!(validate_identifier("my-project_01", "project_id").is_ok());
        assert!(validate_identifier("a", "id").is_ok());
        assert!(validate_identifier("A-B_C", "id").is_ok());
        assert!(validate_identifier("12345", "id").is_ok());
    }

    #[test]
    fn test_validate_identifier_rejects_traversal() {
        assert!(validate_identifier("../etc", "user_id").is_err());
        assert!(validate_identifier("..\\etc", "user_id").is_err());
        assert!(validate_identifier("foo/bar", "user_id").is_err());
        assert!(validate_identifier("..", "user_id").is_err());
        assert!(validate_identifier(".", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_special_chars() {
        assert!(validate_identifier("user id", "user_id").is_err());
        assert!(validate_identifier("user;rm", "user_id").is_err());
        assert!(validate_identifier("user$id", "user_id").is_err());
        assert!(validate_identifier("user@id", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_empty() {
        assert!(validate_identifier("", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_too_long() {
        let long_id = "a".repeat(65);
        assert!(validate_identifier(&long_id, "user_id").is_err());
        let ok_id = "a".repeat(64);
        assert!(validate_identifier(&ok_id, "user_id").is_ok());
    }
}

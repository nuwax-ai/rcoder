//! 共享 garde 校验规则 (对齐 TS nuwax 语义)。
//!
//! handler 层 DTO 经 `#[derive(Validate)]` + `#[garde(custom(...))]` 声明式引用；
//! 错误消息不带字段名, 由 `error::from_garde` 拼成 `"{field}: {message}"`。
//!
//! 用法约定:
//! - `String` 必填非空 → `#[garde(custom(not_blank))]` (trim 后空 → "cannot be empty")
//! - `Option<String>` 必填非空 → `#[garde(custom(required_not_blank))]`
//!   (garde 的 `custom` 不像内置规则那样自动解包 `Option`, 故需单函数处理)
//! - `Option<T>` 必填 (文件等) → 内置 `#[garde(required)]` (None → "not set")

use garde::Error;

/// trim 后非空 (对齐 TS normalizeValue / 空串校验语义: 纯空白也视为空)。
pub fn not_blank(value: &str, _: &()) -> garde::Result {
    if value.trim().is_empty() {
        return Err(Error::new("cannot be empty"));
    }
    Ok(())
}

/// Option<String> 必填且 trim 后非空 (对齐 TS "X is required" 语义)。
///
/// 注: garde 的 `custom` 规则不像内置规则那样自动解包 `Option` —— custom 函数
/// 收到的是 `&Option<String>` 而非 `&str`, 故需单独处理。None 或纯空白都拒绝。
/// (garde 内置 `required` 仅校验 `Some`, 无法表达 "内部 trim 后非空"。)
pub fn required_not_blank(value: &Option<String>, _: &()) -> garde::Result {
    match value.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Ok(()),
        None => Err(Error::new("is required")),
        // 提供了但纯空白 → "cannot be empty" (与 not_blank 语义一致)
        Some(_) => Err(Error::new("cannot be empty")),
    }
}

/// 必填正整数 (对齐 TS `requirePositiveInt`: 由网关传入, 不设默认值)。
/// 仅接受 trim 后解析成功且 > 0 的十进制整数。
pub fn positive_int(value: &str, _: &()) -> garde::Result {
    let ok = value.trim().parse::<usize>().is_ok_and(|n| n > 0);
    if !ok {
        return Err(Error::new("must be a positive integer"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_msg(result: garde::Result) -> String {
        result
            .expect_err("expected validation error")
            .message()
            .to_string()
    }

    #[test]
    fn not_blank_rejects_empty_and_whitespace() {
        assert_eq!(err_msg(not_blank("", &())), "cannot be empty");
        assert_eq!(err_msg(not_blank("   ", &())), "cannot be empty");
        assert_eq!(err_msg(not_blank("\t\n", &())), "cannot be empty");
    }

    #[test]
    fn not_blank_accepts_trimmed_content() {
        assert!(not_blank("a", &()).is_ok());
        assert!(not_blank("  src/main.rs  ", &()).is_ok());
    }

    #[test]
    fn required_not_blank_rejects_none_and_whitespace() {
        assert_eq!(err_msg(required_not_blank(&None, &())), "is required");
        // 提供了但纯空白 → "cannot be empty" (与 not_blank 语义一致)
        assert_eq!(
            err_msg(required_not_blank(&Some("  ".to_string()), &())),
            "cannot be empty"
        );
        assert!(required_not_blank(&Some("abc".to_string()), &()).is_ok());
    }

    #[test]
    fn positive_int_rejects_zero_non_numeric_and_negative() {
        assert_eq!(
            err_msg(positive_int("0", &())),
            "must be a positive integer"
        );
        assert_eq!(
            err_msg(positive_int("abc", &())),
            "must be a positive integer"
        );
        assert_eq!(
            err_msg(positive_int("-1", &())),
            "must be a positive integer"
        );
        assert_eq!(
            err_msg(positive_int("1.5", &())),
            "must be a positive integer"
        );
        assert_eq!(
            err_msg(positive_int("  ", &())),
            "must be a positive integer"
        );
    }

    #[test]
    fn positive_int_accepts_positive_integer_with_surrounding_space() {
        assert!(positive_int("1", &()).is_ok());
        assert!(positive_int(" 100 ", &()).is_ok());
        assert!(positive_int("1000000", &()).is_ok());
    }
}

//! 日志文本摘要工具 —— 平台统一的"净化 + 截断 + 超长标注"标准实现。
//!
//! 供日志/诊断输出场景使用，保证：
//! - 单行完整性：控制字符（换行/制表等）替换为空格，避免一行日志裂成多行
//! - 多字节安全：按字符数截断（中文/emoji 不会截出乱码）
//! - 超长可追溯：截断后追加 `…(+N)` 标注被丢弃的字符数

/// 日志摘要：控制字符替换为空格，超长按字符数截断并追加 `…(+N)` 标注。
///
/// # Arguments
/// * `s`    - 原始文本
/// * `max`  - 最大保留字符数（不含后缀）
///
/// # Examples
/// ```
/// use shared_types::log_brief::brief_log;
/// assert_eq!(brief_log("hello", 80), "hello");
/// assert_eq!(brief_log("a\nb", 80), "a b");
/// assert_eq!(brief_log("abcde", 3), "abc…(+2)");
/// ```
pub fn brief_log(s: &str, max: usize) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let n = sanitized.chars().count();
    if n <= max {
        sanitized
    } else {
        format!(
            "{}…(+{})",
            sanitized.chars().take(max).collect::<String>(),
            n - max
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_text() {
        assert_eq!(brief_log("hello", 80), "hello");
    }

    #[test]
    fn exact_max_unchanged() {
        let s = "a".repeat(80);
        assert_eq!(brief_log(&s, 80), s);
    }

    #[test]
    fn truncates_long_text_with_suffix() {
        let s = "a".repeat(100);
        assert_eq!(brief_log(&s, 80), format!("{}…(+20)", "a".repeat(80)));
    }

    #[test]
    fn counts_chars_not_bytes() {
        // 90 个中文字符（270 字节）→ 截到 80 字符
        let s = "招".repeat(90);
        let out = brief_log(&s, 80);
        assert_eq!(out.chars().count(), 86); // 80 个中文 + '…' + "(+10)" 5 字符
        assert!(out.contains("…(+10)"));
    }

    #[test]
    fn empty_string() {
        assert_eq!(brief_log("", 80), "");
    }

    #[test]
    fn sanitizes_control_chars() {
        assert_eq!(brief_log("a\nb\rc\td", 80), "a b c d");
        assert_eq!(brief_log("标题\n第二行", 80), "标题 第二行");
    }

    #[test]
    fn zero_max_keeps_suffix_only() {
        let out = brief_log("abc", 0);
        assert_eq!(out, "…(+3)");
    }
}

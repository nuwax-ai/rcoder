//! 日志辅助：入参字符串的安全预览等纯格式化函数。

/// prompt 日志预览的前/后字符数（按字符边界截取，不会切坏 UTF-8）。
pub(crate) const PROMPT_LOG_EDGE_CHARS: usize = 100;

/// prompt 预览：首尾各 `edge_chars` 个字符，中间以 `…` 省略；
/// 总长度由调用方与预览一起记录。短于两倍阈值时原样返回。
pub(crate) fn prompt_preview(prompt: &str, edge_chars: usize) -> String {
    let total = prompt.chars().count();
    if total <= edge_chars.saturating_mul(2) {
        return prompt.to_string();
    }
    let head: String = prompt.chars().take(edge_chars).collect();
    let tail: String = prompt.chars().skip(total - edge_chars).collect();
    format!("{head} … {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_prompts_intact() {
        assert_eq!(prompt_preview("hello", 100), "hello");
    }

    #[test]
    fn keeps_head_and_tail_only() {
        let prompt = "0123456789abcdef";
        assert_eq!(prompt_preview(prompt, 4), "0123 … cdef");
    }

    #[test]
    fn respects_utf8_char_boundaries() {
        // 按字符而非字节截取，中文不会被切坏
        assert_eq!(prompt_preview("你好世界你好世界加油", 2), "你好 … 加油");
    }

    #[test]
    fn truncates_only_past_two_edges() {
        assert_eq!(prompt_preview("abcdef", 3), "abcdef");
        assert_eq!(prompt_preview("abcdefg", 3), "abc … efg");
    }
}

//! 国际化 (i18n) 模块
//!
//! 基于 rust-i18n 实现多语言支持

// 导入 rust-i18n 生成的运行时翻译函数
use crate::_rust_i18n_translate;

/// 支持的语言列表
pub const SUPPORTED_LOCALES: &[&str] = &["zh-CN", "zh-TW", "en-US"];

/// 默认语言
pub const DEFAULT_LOCALE: &str = "en-US";

/// 获取翻译消息（运行时版本）
///
/// 使用 rust-i18n 提供的运行时翻译函数
///
/// # Arguments
/// * `key` - 翻译 key，如 "error.agent_busy"
/// * `locale` - 语言代码，如 "zh-CN", "en-US"
///
/// # Returns
/// 翻译后的字符串，如果找不到则返回 key 本身
pub fn t(key: &str, locale: &str) -> String {
    // 验证 locale 是否有效，无效则使用默认值
    let locale = if SUPPORTED_LOCALES.contains(&locale) {
        locale
    } else {
        DEFAULT_LOCALE
    };

    // 使用 rust-i18n 运行时翻译函数
    _rust_i18n_translate(locale, key).to_string()
}

/// 获取默认语言的翻译消息
///
/// # Arguments
/// * `key` - 翻译 key，如 "error.agent_busy"
///
/// # Returns
/// 默认语言的翻译字符串
pub fn t_default(key: &str) -> String {
    t(key, DEFAULT_LOCALE)
}

/// 设置全局语言（用于线程本地存储）
pub fn set_locale(locale: &str) {
    let locale = if SUPPORTED_LOCALES.contains(&locale) {
        locale
    } else {
        DEFAULT_LOCALE
    };
    rust_i18n::set_locale(locale);
}

/// 获取当前语言
pub fn get_locale() -> String {
    rust_i18n::locale().to_string()
}

/// 从 HTTP Accept-Language 头解析语言
///
/// # Arguments
/// * `accept_language` - Accept-Language 头的值
///
/// # Returns
/// 优先级最高的支持语言，如果没有匹配则返回默认语言
pub fn parse_accept_language(accept_language: Option<&str>) -> &'static str {
    match accept_language {
        Some(header) => {
            // 解析 Accept-Language，支持 q 权重（例如: zh-CN,zh;q=0.9,en;q=0.8）
            let mut parts: Vec<(&str, f32, usize)> = header
                .split(',')
                .enumerate()
                .map(|(idx, part)| {
                    let mut lang = "";
                    let mut q = 1.0_f32;

                    for (i, seg) in part.split(';').enumerate() {
                        let seg = seg.trim();
                        if i == 0 {
                            lang = seg;
                            continue;
                        }
                        if let Some(value) = seg.strip_prefix("q=")
                            && let Ok(parsed) = value.parse::<f32>() {
                                q = parsed.clamp(0.0, 1.0);
                            }
                    }

                    (lang, q, idx)
                })
                .collect();

            // 按 q 值降序，同权重按原始顺序
            parts.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.2.cmp(&b.2))
            });

            for (lang, _q, _idx) in parts {
                // 尝试匹配完整语言标签
                match lang {
                    // 简体中文
                    "zh-CN" | "zh-Hans" | "zh-Hans-CN" => return "zh-CN",
                    // 繁体中文 (台湾、香港等)
                    "zh-TW" | "zh-HK" | "zh-Hant" | "zh-Hant-TW" | "zh-Hant-HK" => return "zh-TW",
                    // 英文
                    "en-US" | "en-GB" | "en" => return "en-US",
                    _ => {}
                }
                // 尝试匹配主语言（如 zh-CN -> zh）
                let main_lang = lang.split('-').next().unwrap_or("");
                match main_lang {
                    "zh" => return "zh-CN", // 默认简体
                    "en" => return "en-US",
                    _ => {}
                }
            }
            DEFAULT_LOCALE
        }
        None => DEFAULT_LOCALE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_zh_cn() {
        let msg = t("error.agent_busy", "zh-CN");
        assert_eq!(msg, "Agent 正在执行任务");
    }

    #[test]
    fn test_translate_zh_tw() {
        let msg = t("error.agent_busy", "zh-TW");
        assert_eq!(msg, "Agent 正在執行任務");
    }

    #[test]
    fn test_translate_en_us() {
        let msg = t("error.agent_busy", "en-US");
        assert_eq!(msg, "Agent is busy processing");
    }

    #[test]
    fn test_fallback_locale() {
        let msg = t("error.agent_busy", "invalid-locale");
        // 应该回退到默认语言 en-US
        assert_eq!(msg, "Agent is busy processing");
    }

    #[test]
    fn test_parse_accept_language() {
        // 直接匹配
        assert_eq!(parse_accept_language(Some("zh-CN")), "zh-CN");
        assert_eq!(parse_accept_language(Some("zh-TW")), "zh-TW");
        assert_eq!(parse_accept_language(Some("en-US")), "en-US");

        // 带权重
        assert_eq!(parse_accept_language(Some("zh-CN,zh;q=0.9")), "zh-CN");
        assert_eq!(parse_accept_language(Some("en-US,en;q=0.9")), "en-US");
        assert_eq!(parse_accept_language(Some("en;q=0.8,zh-TW;q=0.9")), "zh-TW");
        assert_eq!(
            parse_accept_language(Some("zh-CN;q=0.7,en-US;q=0.8")),
            "en-US"
        );

        // 繁体中文变体
        assert_eq!(parse_accept_language(Some("zh-HK")), "zh-TW");
        assert_eq!(parse_accept_language(Some("zh-Hant")), "zh-TW");
        assert_eq!(parse_accept_language(Some("zh-Hant-TW")), "zh-TW");

        // 简体中文变体
        assert_eq!(parse_accept_language(Some("zh-Hans")), "zh-CN");

        // 主语言匹配
        assert_eq!(parse_accept_language(Some("zh")), "zh-CN");
        assert_eq!(parse_accept_language(Some("en")), "en-US");

        // 无语言头 -> 默认 en-US
        assert_eq!(parse_accept_language(None), DEFAULT_LOCALE);

        // 不支持的语言 -> 默认 en-US
        assert_eq!(parse_accept_language(Some("fr")), DEFAULT_LOCALE);
    }
}

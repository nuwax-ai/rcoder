//! 构建错误解析 (对齐 nuwax `BuildErrorParser`)。
//!
//! 用一组正则匹配常见构建错误 (模块缺失 / 语法错 / TS 类型错 / 依赖缺失等),
//! 返回用户友好提示; 命中失败回退通用文案。简化版 (nuwax 覆盖更多模式,
//! 后续按需补充)。

use regex::Regex;

/// 解析错误信息 → 用户友好提示 (对齐 nuwax parseBuildError 返回字符串)。
pub fn parse(error_message: &str) -> String {
    let patterns: &[(Pattern, &str)] = &[
        // 模块缺失: Cannot find module 'X' / Can't resolve 'X' (引号可选, 覆盖裸名场景)
        (
            Pattern::new(r#"(?:Cannot find module|Can't resolve|Module not found)[^\n]*?['"`]?([A-Za-z0-9_@/.\-]+)['"`]?"#),
            "Module not found: {1}. 请确认依赖已安装 (pnpm install) 或导入路径正确。",
        ),
        // TS 类型/编译错: error TS1234:
        (
            Pattern::new(r"error TS(\d+):([^\n]*)"),
            "TypeScript 编译错误 (TS{1}):{2}",
        ),
        // 语法错
        (
            Pattern::new(r"SyntaxError:([^\n]*)"),
            "语法错误:{1}",
        ),
        // 依赖安装失败
        (
            Pattern::new(r#"(?:ERR_PNPM|npm ERR!).*?(?:no matching version|not found)[^\n]*['"`]?([A-Za-z0-9_@/.\-]+)['"`]?"#),
            "依赖安装失败: 找不到包 {1} 的匹配版本。",
        ),
    ];
    for (pat, template) in patterns {
        if let Some(caps) = pat.captures(error_message) {
            return apply_template(template, caps);
        }
    }
    "Build failed, please check the detailed error information in the dev/build log."
        .to_string()
}

fn apply_template(template: &str, caps: regex::Captures<'_>) -> String {
    let mut out = template.to_string();
    // 替换 {1} {2} ... 为捕获组 (1-based)
    let mut i = 1;
    while out.contains(&format!("{{{i}}}")) {
        let repl = caps.get(i).map(|m| m.as_str().trim()).unwrap_or("");
        out = out.replace(&format!("{{{i}}}"), repl);
        i += 1;
    }
    out
}

/// 正则封装 (构造一次, lazy; 简化为每次构造, dev 场景可接受)。
struct Pattern(Regex);
impl Pattern {
    fn new(pat: &str) -> Self {
        // 失败回退永不匹配的正则 (理论不会, 模式写死)
        Self(Regex::new(pat).unwrap_or_else(|_| Regex::new(r"$^").unwrap()))
    }
    fn captures<'a>(&self, s: &'a str) -> Option<regex::Captures<'a>> {
        self.0.captures(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_not_found() {
        let m = parse("Error: Cannot find module 'react-dom'");
        assert!(m.contains("react-dom"));
        assert!(m.contains("Module not found"));
    }

    #[test]
    fn ts_error() {
        let m = parse("src/App.tsx(10,5): error TS2322: Type 'string' is not assignable");
        assert!(m.contains("TS2322"));
    }

    #[test]
    fn syntax_error() {
        let m = parse("SyntaxError: Unexpected token in JSON");
        assert!(m.contains("语法错误"));
    }

    #[test]
    fn fallback() {
        let m = parse("some weird unrelated error");
        assert!(m.contains("Build failed"));
    }
}

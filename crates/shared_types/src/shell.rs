//! 进 shell 命令行的字面量转义（与具体业务无关的通用工具）。

/// POSIX sh / bash 的单引号字面量：内容原样、不做任何展开；内嵌单引号用
/// `'\''` 表示（闭合 → 转义的单引号 → 重开）。
///
/// 适用于把一个**完整的参数值**交给 `sh -c` / `bash -c` 的命令行。它只保证
/// 这个参数的边界与内容不被 shell 改写；参数本身是否合法（路径穿越、注入
/// 到被调用程序等）由调用方先行校验——转义不是校验。
pub fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::single_quote;

    #[test]
    fn plain_value_is_wrapped_verbatim() {
        assert_eq!(single_quote("/src/u_1"), "'/src/u_1'");
        assert_eq!(single_quote(""), "''");
    }

    #[test]
    fn embedded_single_quote_is_closed_escaped_and_reopened() {
        assert_eq!(single_quote("it's"), "'it'\\''s'");
        // 连续两个单引号：逐个转义
        assert_eq!(single_quote("a''b"), "'a'\\'''\\''b'");
    }

    /// 用真实 sh 做往返：被包裹的值作为**一个**参数原样到达，内容里的
    /// 元字符不被展开。手写期望字符串容易把转义写错，语义以这条为准。
    #[cfg(unix)]
    #[test]
    fn quoted_value_round_trips_through_a_real_shell() {
        for value in [
            "it's",
            "a''b",
            "u 1;rm -rf /",
            "$(id)",
            "`id`",
            "core.[0-9]*",
            "a\nb",
            "$HOME",
        ] {
            let script = format!("printf '%s' {}", single_quote(value));
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stderr(std::process::Stdio::inherit())
                .output()
                .unwrap();
            assert!(output.status.success(), "sh -c 执行失败: {value:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                value,
                "经 sh 往返后必须原样"
            );
        }
    }
}

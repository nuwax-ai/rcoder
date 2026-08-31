//! dev server / 一次性命令的 stdout/stderr 日志管道 + vite 噪音过滤。

use std::path::PathBuf;

use chrono::Local;

/// vite 在 DEBUG 模式（环境变量 `DEBUG` 含 `vite:*`）下，会把解析后的完整 config 对象
/// dump 到 stderr，例如：
/// ```text
/// 2026-08-02T12:53:31.651Z vite:config using resolved config: {
///   ...
///   logger: {
///     info: [Function: info],
///     ...
///     hasErrorLogged: [Function: hasErrorLogged]
///   },
///   packageCache: Map(1) { ... }
/// }
/// ```
/// 这些 `[Function: ...]` 噪音被 `pipe_stream` 写进 dev 日志后，前端会把它当错误捞起来
/// 发给 agent 处理，造成无谓排查。本过滤器检测 dump 起始行（`<ISO> vite:<ns>` 且本行
/// 打开对象），按花括号深度跳过整个对象体；**单行 vite debug 与正常 vite 输出不受影响**。
struct ViteNoiseFilter {
    /// >0 表示正处在 vite config dump 对象体内部，按 `{`/`}` 深度判断何时结束。
    depth: i32,
}

impl ViteNoiseFilter {
    const fn new() -> Self {
        Self { depth: 0 }
    }

    /// 该行是否应被丢弃（属于 vite debug 对象 dump）。
    fn should_skip(&mut self, line: &str) -> bool {
        if self.depth > 0 {
            self.depth += brace_delta(line);
            if self.depth <= 0 {
                self.depth = 0;
            }
            return true;
        }
        // dump 起始：<ISO> vite:<ns> 开头，且本行净 `{` > 0（打开了对象）
        if is_vite_debug_line(line) && brace_delta(line) > 0 {
            self.depth = brace_delta(line);
            return true;
        }
        false
    }
}

/// 形如 `2026-08-02T12:53:31.651Z vite:config ...`：以数字（ISO 时间戳）开头，
/// 前 40 字符内出现 ` vite:`（带前导空格，区别于路径里的 vite）。
fn is_vite_debug_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 8 || !bytes[0].is_ascii_digit() {
        return false;
    }
    line[..line.len().min(40)].contains(" vite:")
}

/// 本行 `{` 与 `}` 的净数量（跟踪对象 dump 何时闭合）。
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    for c in line.chars() {
        match c {
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

pub(super) async fn pipe_stream<R>(
    reader: R,
    main: PathBuf,
    temp: PathBuf,
    on_line: Option<super::OnLineCallback>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    let mut noise = ViteNoiseFilter::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if noise.should_skip(&line) {
            continue;
        }
        // 行回调（原始行、时间戳前缀之前）：与文件写入同源（噪音行同样被过滤），
        // 供上层实时推送 SSE `log` 事件。
        if let Some(cb) = &on_line {
            cb(&line);
        }
        let prefixed = format!("[{}] {}", Local::now().format("%Y/%m/%d %H:%M:%S"), line);
        // 日志写失败告警(#17):磁盘满/权限错误时可见,不再静默吞掉。
        if let Err(e) = crate::service::dev_server::log::append_line(&main, &prefixed).await {
            tracing::warn!(error = %e, "append_line (main log) failed");
        }
        if let Err(e) = crate::service::dev_server::log::append_line(&temp, &prefixed).await {
            tracing::warn!(error = %e, "append_line (temp log) failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ViteNoiseFilter;

    #[test]
    fn vite_noise_filter_skips_whole_config_dump() {
        let mut f = ViteNoiseFilter::new();
        // dump header 打开对象 → 进入跳过
        assert!(f.should_skip("2026-08-02T12:53:31.651Z vite:config using resolved config: {"));
        // 对象体（噪音，含 hasErrorLogged）
        assert!(f.should_skip("  logger: {"));
        assert!(f.should_skip("    info: [Function: info],"));
        assert!(f.should_skip("    hasErrorLogged: [Function: hasErrorLogged]"));
        assert!(f.should_skip("  },"));
        assert!(f.should_skip("  packageCache: Map(1) {"));
        assert!(f.should_skip("    'fnpd_/app/x' => { dir: '/app/x', data: [Object] }"));
        assert!(f.should_skip("  }"));
        assert!(f.should_skip("}")); // 闭合顶层对象 → depth 回 0
        // dump 结束后，正常行保留
        assert!(!f.should_skip("VITE v5.0.0 ready in 340 ms"));
    }

    #[test]
    fn vite_noise_filter_keeps_normal_and_single_line_debug() {
        let mut f = ViteNoiseFilter::new();
        assert!(!f.should_skip("VITE v5.0.0 ready in 340 ms"));
        assert!(!f.should_skip("[vite] hmr update /src/App.tsx"));
        // 单行 vite debug（没打开对象）不抑制
        assert!(!f.should_skip(
            "2026-08-02T12:53:31.632Z vite:config bundled config file loaded in 373ms"
        ));
        assert!(!f.should_skip("普通应用日志"));
    }

    #[test]
    fn vite_noise_filter_ignores_non_vite_braces() {
        let mut f = ViteNoiseFilter::new();
        // 非 vite debug 行即使带花括号也不触发
        assert!(!f.should_skip("12:00:00 app: render {"));
        assert!(!f.should_skip("  data: 1"));
        assert!(!f.should_skip("}"));
    }
}

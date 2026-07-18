//! Dev server 启动错误分类 (回应"command 报错难解析"的顾虑)。
//!
//! 设计借鉴:
//! - **vite-rs**: stderr/stdout 分流 (vite 错误走 stderr), 故从 stderr 末尾分类;
//! - **farm**: 类型化错误枚举 (非裸字符串);
//! - 复用 [`crate::service::build_error`] 的依赖缺失 pattern。
//!
//! vite 启动失败时进程秒退, stderr 会留下结构化报错 (端口占用/配置错/缺依赖/transform 错)。
//! 本模块把 stderr 末尾几行 pattern 匹配成 [`ViteStartupError`], 转成可操作的 [`AppError`]。

use crate::error::AppError;

use regex::Regex;
use std::sync::LazyLock;

static PORT_IN_USE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile_regex(r"(?i)port (\d+) is (?:in use|already in use)|EADDRINUSE.*?(\d+)")
});
static NODE_MISSING_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile_regex(
        r"(?i)(?:npx|node): (?:command )?not found|no such file or directory.*npx|enoent.*spawn|cannot run node",
    )
});
static MISSING_MODULE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)(?:Cannot find module|Failed to resolve (?:import )?|Module not found)\s*['"]?([A-Za-z0-9_@/.\-]+)"#,
    )
});
static CONFIG_ERROR_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile_regex(
        r"(?im)(Failed to load config[^\n]*|vite\.config[^\n]*SyntaxError[^\n]*|SyntaxError[^\n]*vite\.config[^\n]*|[Ee]rror in config[^\n]*)",
    )
});
static TRANSFORM_ERROR_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile_regex(
        r"(?im)(Pre-transform error[^\n]*|Internal server error[^\n]*|[Ee]rror during transform[^\n]*)",
    )
});

/// stderr 环形缓冲 (启动期间保留末尾若干行供分类)。
pub type StderrRing = std::sync::Mutex<std::collections::VecDeque<String>>;

/// 环形缓冲容量 (末尾 N 行足够分类, 不堆积)。
pub const STDERR_RING_CAP: usize = 64;

/// vite 启动失败的具体类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViteStartupError {
    /// 端口被占用 (--strictPort 下 vite 不会自动换端口, 直接退出)。
    PortInUse { port: u16 },
    /// vite.config.* 加载/语法错。
    ConfigError { detail: String },
    /// 缺少依赖模块。
    DependencyMissing { module: String },
    /// transform / 预编译错 (多为源码语法错)。
    TransformError { detail: String },
    /// 找不到 node/npx (容器环境问题)。
    NodeMissing,
    /// 未知错误, 附 stderr 末尾。
    Unknown { tail: String },
}

impl ViteStartupError {
    /// 按 stderr 行分类 (从后往前匹配, 命中即返回)。
    pub fn classify(lines: &[String]) -> Self {
        // 合并成单串便于跨行 pattern, 同时保留行级匹配
        let joined = lines.join("\n");
        if let Some(p) = capture_port(&joined) {
            return Self::PortInUse { port: p };
        }
        if is_node_missing(&joined) {
            return Self::NodeMissing;
        }
        if let Some(m) = capture_module(&joined) {
            return Self::DependencyMissing { module: m };
        }
        if let Some(d) = capture_config(&joined) {
            return Self::ConfigError { detail: d };
        }
        if let Some(d) = capture_transform(&joined) {
            return Self::TransformError { detail: d };
        }
        Self::Unknown {
            tail: tail_lines(lines, 8),
        }
    }

    /// 转成可操作的 AppError (system 级, 带定位与建议)。
    pub fn into_app_error(self, pid: u32, port: u16) -> AppError {
        match self {
            Self::PortInUse { port: p } => AppError::system(format!(
                "dev server (pid {pid}) 启动失败: 端口 {p} 被占用 (--strictPort 不自动换端口)。建议清理占用或稍后重试 (分配端口 {port})"
            )),
            Self::ConfigError { detail } => AppError::system(format!(
                "dev server (pid {pid}) 启动失败: vite 配置错误 — {detail}。请检查 vite.config.ts"
            )),
            Self::DependencyMissing { module } => AppError::system(format!(
                "dev server (pid {pid}) 启动失败: 缺少依赖 '{module}'。请先在该项目执行 pnpm install"
            )),
            Self::TransformError { detail } => AppError::system(format!(
                "dev server (pid {pid}) 启动失败: 源码 transform 错误 — {detail}"
            )),
            Self::NodeMissing => AppError::system(format!(
                "dev server (pid {pid}) 启动失败: 找不到 node/npx, 请检查容器 Node.js 环境 (PATH)"
            )),
            Self::Unknown { tail } => AppError::system(format!(
                "dev server (pid {pid}, port {port}) 启动失败 (未识别错误类型)。stderr 末尾:\n{tail}"
            )),
        }
    }
}

fn capture_port(s: &str) -> Option<u16> {
    let caps = PORT_IN_USE_RE.as_ref()?.captures(s)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .and_then(|m| m.as_str().parse().ok())
}

fn is_node_missing(s: &str) -> bool {
    NODE_MISSING_RE
        .as_ref()
        .is_some_and(|regex| regex.is_match(s))
}

fn capture_module(s: &str) -> Option<String> {
    let caps = MISSING_MODULE_RE.as_ref()?.captures(s)?;
    caps.get(1).map(|m| m.as_str().to_string())
}

fn capture_config(s: &str) -> Option<String> {
    let m = CONFIG_ERROR_RE.as_ref()?.captures(s)?.get(1)?;
    Some(m.as_str().trim().to_string())
}

fn capture_transform(s: &str) -> Option<String> {
    let m = TRANSFORM_ERROR_RE.as_ref()?.captures(s)?.get(1)?;
    Some(m.as_str().trim().to_string())
}

fn compile_regex(pattern: &'static str) -> Option<Regex> {
    Regex::new(pattern)
        .map_err(|error| {
            tracing::error!(%error, %pattern, "invalid built-in Vite error regex");
        })
        .ok()
}

/// 取末尾 N 行 (用于 Unknown 兜底展示)。
fn tail_lines(lines: &[String], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// 往环形缓冲推一行 (满则弹出最旧; 锁失败静默跳过)。
pub fn ring_push(ring: &StderrRing, line: &str) {
    if let Ok(mut buf) = ring.lock() {
        if buf.len() >= STDERR_RING_CAP {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
    }
}

/// 收集环形缓冲全部行 (供分类; 锁失败返回空)。
pub fn ring_collect(ring: &StderrRing) -> Vec<String> {
    ring.lock()
        .map(|buf| buf.iter().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn classifies_port_in_use() {
        let e = ViteStartupError::classify(&lines(
            "VITE v5 ready\nError: Port 5173 is in use, trying another one...",
        ));
        assert_eq!(e, ViteStartupError::PortInUse { port: 5173 });
    }

    #[test]
    fn classifies_dependency_missing() {
        let e = ViteStartupError::classify(&lines("Error: Cannot find module 'react-dom'"));
        assert_eq!(
            e,
            ViteStartupError::DependencyMissing {
                module: "react-dom".into()
            }
        );
    }

    #[test]
    fn classifies_resolve_import() {
        let e = ViteStartupError::classify(&lines(
            "x Failed to resolve import \"@/foo\" from App.tsx.",
        ));
        match e {
            ViteStartupError::DependencyMissing { module } => assert_eq!(module, "@/foo"),
            other => panic!("expected DependencyMissing, got {other:?}"),
        }
    }

    #[test]
    fn classifies_config_error() {
        let e = ViteStartupError::classify(&lines("Failed to load config from vite.config.ts"));
        assert!(matches!(e, ViteStartupError::ConfigError { .. }));
    }

    #[test]
    fn classifies_node_missing() {
        let e = ViteStartupError::classify(&lines("sh: npx: command not found"));
        assert_eq!(e, ViteStartupError::NodeMissing);
    }

    #[test]
    fn classifies_unknown_fallback() {
        let e = ViteStartupError::classify(&lines("some weird\nunrelated output\nexit code 1"));
        assert!(matches!(e, ViteStartupError::Unknown { .. }));
    }

    #[test]
    fn app_error_messages_are_actionable() {
        let msg = ViteStartupError::DependencyMissing {
            module: "foo".into(),
        }
        .into_app_error(123, 4000);
        let s = match msg {
            AppError::System(m) => m,
            _ => panic!("expected System"),
        };
        assert!(s.contains("foo"));
        assert!(s.contains("pnpm install"));
    }
}

//! 终端输出格式化
//!
//! 提供带颜色的终端输出，自动检测 TTY 状态以决定是否输出 ANSI 颜色码。
//! 当输出被重定向到文件或管道时，自动禁用颜色以避免乱码。

use std::io::IsTerminal;

/// ANSI 颜色代码
#[allow(dead_code)]
mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
    pub const WHITE: &str = "\x1b[37m";
}

/// 输出级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLevel {
    /// 静默模式：仅输出 agent 内容
    Quiet,
    /// 正常模式：状态信息 + agent 内容
    Normal,
    /// 详细模式：额外调试信息
    Verbose,
    /// 超详细模式：原始事件
    Trace,
}

impl OutputLevel {
    pub fn from_verbose_count(count: u8, quiet: bool) -> Self {
        if quiet {
            OutputLevel::Quiet
        } else {
            match count {
                0 => OutputLevel::Normal,
                1 => OutputLevel::Verbose,
                _ => OutputLevel::Trace,
            }
        }
    }
}

/// 终端输出格式化器
///
/// 自动检测 stderr 是否为 TTY，非 TTY 时禁用 ANSI 颜色码。
/// 可通过 `with_color()` 显式覆盖（例如 `--no-color` 参数）。
pub struct OutputFormatter {
    level: OutputLevel,
    color: bool,
}

impl OutputFormatter {
    pub fn new(level: OutputLevel) -> Self {
        Self {
            level,
            color: std::io::stderr().is_terminal(),
        }
    }

    /// 显式设置颜色输出开关（用于 `--no-color` 或 `--color=always` 参数）。
    #[allow(dead_code)]
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// 输出信息提示（蓝色）
    pub fn info(&self, msg: &str) {
        if self.level != OutputLevel::Quiet {
            if self.color {
                eprintln!(
                    "{}{}[INFO]{} {}",
                    colors::BLUE,
                    colors::BOLD,
                    colors::RESET,
                    msg
                );
            } else {
                eprintln!("[INFO] {}", msg);
            }
        }
    }

    /// 输出成功提示（绿色）
    pub fn success(&self, msg: &str) {
        if self.level != OutputLevel::Quiet {
            if self.color {
                eprintln!(
                    "{}{}[ OK ]{} {}",
                    colors::GREEN,
                    colors::BOLD,
                    colors::RESET,
                    msg
                );
            } else {
                eprintln!("[ OK ] {}", msg);
            }
        }
    }

    /// 输出警告提示（黄色）
    pub fn warn(&self, msg: &str) {
        if self.level != OutputLevel::Quiet {
            if self.color {
                eprintln!(
                    "{}{}[WARN]{} {}",
                    colors::YELLOW,
                    colors::BOLD,
                    colors::RESET,
                    msg
                );
            } else {
                eprintln!("[WARN] {}", msg);
            }
        }
    }

    /// 输出错误提示（红色）
    pub fn error(&self, msg: &str) {
        // Errors always shown, even in quiet mode
        if self.color {
            eprintln!(
                "{}{}[ERR ]{} {}",
                colors::RED,
                colors::BOLD,
                colors::RESET,
                msg
            );
        } else {
            eprintln!("[ERR ] {}", msg);
        }
    }

    /// 输出调试信息（仅 verbose 及以上）
    pub fn debug(&self, msg: &str) {
        if matches!(self.level, OutputLevel::Verbose | OutputLevel::Trace) {
            if self.color {
                eprintln!("{}[DBG ] {}{}", colors::DIM, msg, colors::RESET);
            } else {
                eprintln!("[DBG ] {}", msg);
            }
        }
    }

    /// 输出 trace 信息（仅 trace 级别）
    pub fn trace(&self, msg: &str) {
        if self.level == OutputLevel::Trace {
            if self.color {
                eprintln!("{}[TRC ] {}{}", colors::DIM, msg, colors::RESET);
            } else {
                eprintln!("[TRC ] {}", msg);
            }
        }
    }

    /// 输出 agent 内容文本（无装饰）
    pub fn agent_text(&self, text: &str) {
        // Always output agent text regardless of level
        print!("{}", text);
    }

    /// 输出 agent 内容文本并换行
    #[allow(dead_code)]
    pub fn agent_text_ln(&self, text: &str) {
        println!("{}", text);
    }

    /// 输出工具调用信息（品红色）
    pub fn tool_call(&self, tool_name: &str, status: &str) {
        if self.level != OutputLevel::Quiet {
            if self.color {
                eprintln!(
                    "{}[TOOL]{} {} {} {}",
                    colors::MAGENTA, colors::RESET, tool_name, colors::DIM, status
                );
            } else {
                eprintln!("[TOOL] {} {}", tool_name, status);
            }
        }
    }

    /// 输出分隔线
    pub fn separator(&self) {
        if self.level != OutputLevel::Quiet {
            if self.color {
                eprintln!("{}", colors::DIM);
                eprintln!("────────────────────────────────────────────────────────");
                eprintln!("{}", colors::RESET);
            } else {
                eprintln!("────────────────────────────────────────────────────────");
            }
        }
    }

    #[allow(dead_code)]
    pub fn level(&self) -> OutputLevel {
        self.level
    }
}

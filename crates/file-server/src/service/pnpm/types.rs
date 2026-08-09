//! 与具体 pnpm 后端无关的请求、结果和日志类型。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

const MAX_DIAGNOSTICS: usize = 32;

/// pnpm install 可选参数。未建模的稳定 CLI 参数可以通过 [`Self::extra_args`] 传入。
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub prefer_offline: bool,
    pub extra_args: Vec<String>,
}

impl InstallOptions {
    pub fn prefer_offline() -> Self {
        Self {
            prefer_offline: true,
            extra_args: Vec::new(),
        }
    }
}

/// 可选日志目标；dev/build 路径同时写主日志和本次临时日志。
#[derive(Debug, Clone)]
pub struct LogFiles {
    pub main: PathBuf,
    pub temporary: PathBuf,
}

impl LogFiles {
    pub fn new(main: impl Into<PathBuf>, temporary: impl Into<PathBuf>) -> Self {
        Self {
            main: main.into(),
            temporary: temporary.into(),
        }
    }
}

/// 一次安装的结构化摘要。
#[derive(Debug, Clone, Default)]
pub struct InstallSummary {
    pub event_count: usize,
    pub events_by_name: BTreeMap<String, usize>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub store_dir: Option<String>,
    pub warning_count: usize,
    pub error_codes: Vec<String>,
    pub diagnostics: Vec<String>,
}

impl InstallSummary {
    pub(super) fn push_diagnostic(&mut self, message: &str) {
        if self.diagnostics.len() == MAX_DIAGNOSTICS {
            self.diagnostics.remove(0);
        }
        self.diagnostics.push(message.to_string());
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.event_count += other.event_count;
        for (name, count) in other.events_by_name {
            *self.events_by_name.entry(name).or_default() += count;
        }
        self.added = other.added.or(self.added);
        self.removed = other.removed.or(self.removed);
        self.store_dir = other.store_dir.or_else(|| self.store_dir.take());
        self.warning_count += other.warning_count;
        for code in other.error_codes {
            if !self.error_codes.contains(&code) {
                self.error_codes.push(code);
            }
        }
        for diagnostic in other.diagnostics {
            self.push_diagnostic(&diagnostic);
        }
    }
}

/// 成功安装结果。
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub elapsed: Duration,
    pub summary: InstallSummary,
}

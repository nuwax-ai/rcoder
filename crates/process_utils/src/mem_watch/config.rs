//! 看门狗配置与开关解析。

use std::time::Duration;

/// 运行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 完全不工作（不采样、不杀）。
    Off,
    /// 只记录"会杀谁"，不真的杀。
    DryRun,
    /// 真的杀。
    On,
}

impl Mode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "disable" | "disabled" => Some(Self::Off),
            "dry" | "dry-run" | "dryrun" => Some(Self::DryRun),
            "on" | "1" | "true" | "enable" | "enabled" => Some(Self::On),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dry",
            Self::On => "on",
        }
    }
}

/// 运行参数。默认值即生产建议值。
#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    /// 触发阈值（占 `memory.max` 比例，按 working set 计）。
    pub ratio: f64,
    /// 采样周期。
    pub interval: Duration,
    /// 可杀进程的最小 RSS（占 limit 比例）。
    pub min_ratio: f64,
    /// 可杀进程的最小 RSS 绝对下限（小容器保护）。
    pub min_rss_floor: u64,
    /// SIGTERM → SIGKILL 的等待。
    pub grace: Duration,
    /// 两次 kill 之间的冷却。
    pub cooldown: Duration,
    /// 启动窗口兜底：到点强制封窗（没有 `mark_session_active` 的 agent）。
    pub bootstrap_cap: Duration,
    /// 内存限额覆盖（字节）。集群用 host cgroup namespace 时，容器内
    /// `/sys/fs/cgroup/memory.max` 是 `max`，读不到真实限额；由 rcoder 创建
    /// pod 时把已知限额注入 env，看门狗以它为准。
    pub limit_override: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            limit_override: None,
            ratio: 0.85,
            interval: Duration::from_millis(500),
            min_ratio: 0.08,
            min_rss_floor: 64 * 1024 * 1024,
            grace: Duration::from_secs(3),
            cooldown: Duration::from_secs(10),
            bootstrap_cap: Duration::from_secs(300),
        }
    }
}

impl Config {
    /// 从环境变量读取（第 4 层默认值；文件开关优先级更高，见 sampler）。
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            mode: std::env::var("RCODER_MEM_WATCH")
                .ok()
                .and_then(|s| Mode::parse(&s))
                .unwrap_or(d.mode),
            ratio: parse_f64(env_str("RCODER_MEM_WATCH_RATIO").as_deref(), d.ratio),
            interval: Duration::from_millis(parse_u64(
                env_str("RCODER_MEM_WATCH_INTERVAL_MS").as_deref(),
                d.interval.as_millis() as u64,
            )),
            min_ratio: parse_f64(
                env_str("RCODER_MEM_WATCH_MIN_RATIO").as_deref(),
                d.min_ratio,
            ),
            min_rss_floor: parse_u64(
                env_str("RCODER_MEM_WATCH_MIN_RSS_MB").as_deref(),
                d.min_rss_floor / (1024 * 1024),
            )
            .saturating_mul(1024 * 1024),
            grace: Duration::from_millis(parse_u64(
                env_str("RCODER_MEM_WATCH_GRACE_MS").as_deref(),
                d.grace.as_millis() as u64,
            )),
            cooldown: Duration::from_millis(parse_u64(
                env_str("RCODER_MEM_WATCH_COOLDOWN_MS").as_deref(),
                d.cooldown.as_millis() as u64,
            )),
            bootstrap_cap: Duration::from_millis(parse_u64(
                env_str("RCODER_MEM_WATCH_BOOTSTRAP_CAP_MS").as_deref(),
                d.bootstrap_cap.as_millis() as u64,
            )),
            limit_override: env_str("RCODER_MEM_WATCH_LIMIT_BYTES")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|v| *v > 0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.mode != Mode::Off
    }

    /// 可杀进程的 RSS 下限：`max(floor, limit × min_ratio)`。
    pub fn min_rss(&self, limit: u64) -> u64 {
        ((limit as f64 * self.min_ratio) as u64).max(self.min_rss_floor)
    }

    /// 触发阈值（字节）。
    pub fn threshold(&self, limit: u64) -> u64 {
        (limit as f64 * self.ratio) as u64
    }

    /// 生效限额：显式覆盖优先，否则读 cgroup。
    pub fn effective_limit(&self) -> Option<u64> {
        self.limit_override.or_else(super::sys::read_memory_max)
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn parse_u64(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn parse_f64(raw: Option<&str>, default: f64) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_tolerantly() {
        assert_eq!(Mode::parse("ON"), Some(Mode::On));
        assert_eq!(Mode::parse(" dry "), Some(Mode::DryRun));
        assert_eq!(Mode::parse("0"), Some(Mode::Off));
        assert_eq!(Mode::parse("nonsense"), None);
    }

    #[test]
    fn min_rss_respects_floor_and_ratio() {
        let cfg = Config::default();
        // 8GiB × 8% = 655.36MiB > 64MiB
        assert_eq!(
            cfg.min_rss(8 << 30),
            (8f64 * 0.08 * 1024.0 * 1024.0 * 1024.0) as u64
        );
        // 512MiB × 8% = 40MiB < 64MiB → 取 floor
        assert_eq!(cfg.min_rss(512 << 20), 64 * 1024 * 1024);
    }

    #[test]
    fn threshold_matches_ratio() {
        let cfg = Config::default();
        assert_eq!(
            cfg.threshold(8 << 30),
            (8f64 * 0.85 * 1024.0 * 1024.0 * 1024.0) as u64
        );
    }

    #[test]
    fn effective_limit_prefers_override() {
        let mut cfg = Config::default();
        cfg.limit_override = Some(512 * 1024 * 1024);
        assert_eq!(cfg.effective_limit(), Some(512 * 1024 * 1024));
        cfg.limit_override = None;
        // 非 Linux（macOS 测试机）读不到 cgroup → None；Linux 有限制时返回真实值
        let _ = cfg.effective_limit();
    }

    #[test]
    fn rejects_invalid_overrides() {
        assert_eq!(parse_f64(Some("abc"), 0.85), 0.85);
        assert_eq!(parse_f64(Some("2.0"), 0.85), 0.85, "比例必须 <= 1");
        assert_eq!(parse_u64(Some("0"), 500), 500, "0 视为无效");
        assert_eq!(parse_u64(Some("250"), 500), 250);
    }
}

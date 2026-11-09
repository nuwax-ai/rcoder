//! 文件开关：热更新、不用重启容器。
//!
//! 四层来源（优先级从高到低）：
//! 1. `/tmp/mem-watch.conf`             —— 单容器，`kubectl exec` 即改（<1s）
//! 2. `/home/user/.rcoder/mem-watch.conf` —— 单用户（共享 PVC）
//! 3. `/etc/agent-runtime/mem-watch.conf` —— 全集群（ConfigMap 挂载）
//! 4. `RCODER_MEM_WATCH*` 环境变量       —— 启动默认值
//!
//! 读盘频率限制为 1s；内容没变不重新解析。解析失败**保留上一份有效配置**。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::config::{Config, Mode};

/// 默认查找路径（按优先级）。
pub const DEFAULT_PATHS: &[&str] = &[
    "/tmp/mem-watch.conf",
    "/home/user/.rcoder/mem-watch.conf",
    "/etc/agent-runtime/mem-watch.conf",
];

const REFRESH_EVERY: Duration = Duration::from_secs(1);

/// 开关文件里可以覆盖的字段；`None` 表示沿用下一层。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Overrides {
    pub mode: Option<Mode>,
    pub ratio: Option<f64>,
    pub interval_ms: Option<u64>,
    pub min_ratio: Option<f64>,
    pub grace_ms: Option<u64>,
    pub cooldown_ms: Option<u64>,
    pub bootstrap_cap_ms: Option<u64>,
}

impl Overrides {
    pub fn apply(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        if let Some(mode) = self.mode {
            cfg.mode = mode;
        }
        if let Some(ratio) = self.ratio {
            cfg.ratio = ratio;
        }
        if let Some(ms) = self.interval_ms {
            cfg.interval = Duration::from_millis(ms);
        }
        if let Some(ratio) = self.min_ratio {
            cfg.min_ratio = ratio;
        }
        if let Some(ms) = self.grace_ms {
            cfg.grace = Duration::from_millis(ms);
        }
        if let Some(ms) = self.cooldown_ms {
            cfg.cooldown = Duration::from_millis(ms);
        }
        if let Some(ms) = self.bootstrap_cap_ms {
            cfg.bootstrap_cap = Duration::from_millis(ms);
        }
        cfg
    }
}

/// 解析开关文件。未知键忽略；已知键的值非法则报错（调用方保留旧配置）。
pub fn parse(raw: &str) -> Result<Overrides, String> {
    let mut out = Overrides::default();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("第 {} 行缺少 '=': {line}", idx + 1));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        let bad = || format!("第 {} 行取值非法: {line}", idx + 1);
        match key.as_str() {
            "mode" => out.mode = Some(Mode::parse(value).ok_or_else(bad)?),
            "ratio" => out.ratio = Some(ratio(value).ok_or_else(bad)?),
            "min_ratio" => out.min_ratio = Some(ratio(value).ok_or_else(bad)?),
            "interval_ms" => out.interval_ms = Some(millis(value).ok_or_else(bad)?),
            "grace_ms" => out.grace_ms = Some(millis(value).ok_or_else(bad)?),
            "cooldown_ms" => out.cooldown_ms = Some(millis(value).ok_or_else(bad)?),
            "bootstrap_cap_ms" => out.bootstrap_cap_ms = Some(millis(value).ok_or_else(bad)?),
            // protect / 其它扩展键由后续版本消费；这里只做"不报错"
            _ => {}
        }
    }
    Ok(out)
}

fn ratio(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
}

fn millis(raw: &str) -> Option<u64> {
    raw.parse::<u64>().ok().filter(|v| *v > 0)
}

/// 带缓存的开关解析器。sampler 每 tick 调 [`Switch::effective`]。
pub struct Switch {
    paths: Vec<PathBuf>,
    last_refresh: Option<Instant>,
    cached_raw: Option<String>,
    cached: Overrides,
    last_error: Option<String>,
}

impl Default for Switch {
    fn default() -> Self {
        Self::new(DEFAULT_PATHS.iter().map(PathBuf::from).collect())
    }
}

impl Switch {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            last_refresh: None,
            cached_raw: None,
            cached: Overrides::default(),
            last_error: None,
        }
    }

    /// 当前生效配置 = 环境/内置默认 + 开关文件覆盖。
    pub fn effective(&mut self, base: &Config) -> Config {
        self.refresh_if_due();
        self.cached.apply(base)
    }

    fn refresh_if_due(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_refresh {
            if now.duration_since(last) < REFRESH_EVERY {
                return;
            }
        }
        self.last_refresh = Some(now);

        let raw = self.read_first_existing();
        if raw.is_none() {
            // 文件被删除 ⇒ 回到默认
            if self.cached != Overrides::default() {
                self.cached = Overrides::default();
                self.cached_raw = None;
            }
            return;
        }
        let raw = raw.expect("checked above");
        if self.cached_raw.as_deref() == Some(raw.as_str()) {
            return;
        }

        match parse(&raw) {
            Ok(overrides) => {
                self.cached = overrides;
                self.cached_raw = Some(raw);
                self.last_error = None;
            }
            Err(err) => {
                // 半成品/写坏的文件不能改变运行配置
                if self.last_error.as_deref() != Some(err.as_str()) {
                    tracing::error!(event = "mem_watch.switch_invalid", error = %err);
                    self.last_error = Some(err);
                }
            }
        }
    }

    fn read_first_existing(&self) -> Option<String> {
        for path in &self.paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                return Some(content);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_file() {
        let raw = "\
# 注释
mode = ON
ratio = 0.8
interval_ms = 250
min_ratio = 0.05
grace_ms = 1500
cooldown_ms = 5000
bootstrap_cap_ms = 60000
unknown_key = ignored
";
        let o = parse(raw).unwrap();
        assert_eq!(o.mode, Some(Mode::On));
        assert_eq!(o.ratio, Some(0.8));
        assert_eq!(o.interval_ms, Some(250));
        assert_eq!(o.cooldown_ms, Some(5000));
    }

    #[test]
    fn rejects_invalid_value() {
        assert!(parse("ratio = 2.0").is_err());
        assert!(parse("interval_ms = 0").is_err());
        assert!(parse("mode = maybe").is_err());
    }

    #[test]
    fn ignores_unknown_keys() {
        assert_eq!(parse("protect = /opt/x").unwrap(), Overrides::default());
    }

    #[test]
    fn applies_overrides_on_top_of_base() {
        let base = Config::default();
        let o = parse("mode=dry\nratio=0.5").unwrap();
        let cfg = o.apply(&base);
        assert_eq!(cfg.mode, Mode::DryRun);
        assert_eq!(cfg.ratio, 0.5);
        assert_eq!(cfg.interval, base.interval, "未覆盖的字段保持默认");
    }

    #[test]
    fn switch_reads_file_and_survives_broken_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem-watch.conf");
        std::fs::write(&path, "mode=on\n").unwrap();

        let base = Config::default();
        let mut switch = Switch::new(vec![path.clone()]);
        assert_eq!(switch.effective(&base).mode, Mode::On);

        // 写坏：保留上一份有效值
        std::fs::write(&path, "mode=on\nratio=2.0\n").unwrap();
        switch.last_refresh = None; // 跳过 1s 节流，测试用
        assert_eq!(switch.effective(&base).mode, Mode::On);
        assert_eq!(switch.effective(&base).ratio, base.ratio);

        // 修复：重新生效
        std::fs::write(&path, "mode=off\n").unwrap();
        switch.last_refresh = None;
        assert_eq!(switch.effective(&base).mode, Mode::Off);

        // 删除：回到默认
        std::fs::remove_file(&path).unwrap();
        switch.last_refresh = None;
        assert_eq!(switch.effective(&base).mode, base.mode);
    }
}

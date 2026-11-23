//! 文件开关：热更新、不用重启容器。
//!
//! 四层来源（优先级从高到低）：
//! 1. `/tmp/mem-watch.conf`             —— 单容器，`kubectl exec` 即改（<1s）
//! 2. `/home/user/.rcoder/mem-watch.conf` —— 单用户（共享 PVC）
//! 3. `/etc/agent-runtime/mem-watch.conf` —— 全集群（ConfigMap 挂载）
//! 4. `RCODER_MEM_WATCH*` 环境变量       —— 启动默认值
//!
//! 读盘频率限制为 1s；内容没变不重新解析。解析失败**保留上一份有效配置**。
//!
//! # 信任边界
//!
//! 第 1、2 层所在目录对容器内的业务进程可写，而它们正是看门狗要淘汰的对象。
//! 不设防的话，一个失控的进程写一行 `mode=off`（或把 `ratio` / `interval_ms`
//! 调到不起作用）就能让看门狗闭嘴。按**层**划分信任：
//!
//! - **可信层** = 环境变量/内置默认，以及可信目录 [`TRUSTED_DIR`] 下的文件
//!   （ConfigMap 挂载，只有运维能写）。可信层可以任意覆盖。
//! - **其余文件**只能把配置调得**更严**：`mode` 只能沿 off → dry → on 方向
//!   走，数值参数取与可信配置相比更小的那个。它们也遮不住可信层——可信
//!   配置由环境变量与第一个存在的可信文件共同决定。
//!
//! 不能按文件属主判定：agent 容器里的全部进程（agent_runner、agent 拉起的
//! 工具、桌面组件）都以 root 运行，`kubectl exec` 也是 root，业务进程写出的
//! 文件与运维写的无法区分（31.45 实测）。
//!
//! 运维含义：按容器用 `/tmp` 在 dry ↔ on 之间切换的灰度流程不变（基线是
//! off 或 dry）；一旦把基线设成 `on`，容器内的文件就关不掉它，回滚要改
//! ConfigMap 或环境变量——所以全量开启前先把第 3 层挂上。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::{Config, Mode};

/// 默认查找路径（按优先级）。
pub const DEFAULT_PATHS: &[&str] = &[
    "/tmp/mem-watch.conf",
    "/home/user/.rcoder/mem-watch.conf",
    "/etc/agent-runtime/mem-watch.conf",
];

/// 可信目录：ConfigMap 挂载点。只有运维能写，容器内的业务进程改不了。
pub const TRUSTED_DIR: &str = "/etc/agent-runtime";

const REFRESH_EVERY: Duration = Duration::from_secs(1);

/// 开关文件是否属于可信层（可以任意覆盖配置）：位于 [`TRUSTED_DIR`] 之下。
///
/// 按路径分量比较，`/etc/agent-runtime-evil/…` 不算；含 `..` / `.` 分量的
/// 路径一律不算——`Path::starts_with` 不做规范化，
/// `/etc/agent-runtime/../tmp/x` 会被它当成可信目录之下。
pub fn is_trusted_path(path: &Path) -> bool {
    use std::path::Component;
    let escapes = path.components().any(|component| match component {
        Component::Prefix(_) | Component::RootDir | Component::Normal(_) => false,
        Component::CurDir | Component::ParentDir => true,
    });
    !escapes && path.starts_with(TRUSTED_DIR)
}

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

    /// 只往更严的方向覆盖（见模块文档"信任边界"）：`mode` 取更严的，
    /// 数值参数取与可信配置相比更小的那个。
    fn tighten(&self, trusted: &Config) -> Config {
        let mut cfg = trusted.clone();
        if let Some(mode) = self.mode {
            cfg.mode = stricter(cfg.mode, mode);
        }
        if let Some(ratio) = self.ratio {
            cfg.ratio = cfg.ratio.min(ratio);
        }
        if let Some(ms) = self.interval_ms {
            cfg.interval = cfg.interval.min(Duration::from_millis(ms));
        }
        if let Some(ratio) = self.min_ratio {
            cfg.min_ratio = cfg.min_ratio.min(ratio);
        }
        if let Some(ms) = self.grace_ms {
            cfg.grace = cfg.grace.min(Duration::from_millis(ms));
        }
        if let Some(ms) = self.cooldown_ms {
            cfg.cooldown = cfg.cooldown.min(Duration::from_millis(ms));
        }
        if let Some(ms) = self.bootstrap_cap_ms {
            cfg.bootstrap_cap = cfg.bootstrap_cap.min(Duration::from_millis(ms));
        }
        cfg
    }
}

/// 两种模式里更严的那个：off < dry < on。
fn stricter(current: Mode, requested: Mode) -> Mode {
    let rank = |mode: Mode| match mode {
        Mode::Off => 0,
        Mode::DryRun => 1,
        Mode::On => 2,
    };
    if rank(requested) > rank(current) {
        requested
    } else {
        current
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
    trusted: fn(&Path) -> bool,
    last_refresh: Option<Instant>,
    /// 当前生效的覆盖：来自优先级最高的那个**存在**的文件。
    cached_raw: Option<String>,
    cached: Overrides,
    cached_trusted: bool,
    /// 当前文件不可信时作为基线的可信覆盖：更低优先级里第一个存在的可信文件。
    trusted_raw: Option<String>,
    trusted_cached: Option<Overrides>,
    last_error: Option<String>,
}

impl Default for Switch {
    fn default() -> Self {
        Self::new(DEFAULT_PATHS.iter().map(PathBuf::from).collect())
    }
}

impl Switch {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self::with_trust(paths, is_trusted_path)
    }

    /// `trusted` 判定一个开关文件能否任意覆盖配置（默认：位于可信目录下）。
    pub fn with_trust(paths: Vec<PathBuf>, trusted: fn(&Path) -> bool) -> Self {
        Self {
            paths,
            trusted,
            last_refresh: None,
            cached_raw: None,
            cached: Overrides::default(),
            cached_trusted: false,
            trusted_raw: None,
            trusted_cached: None,
            last_error: None,
        }
    }

    /// 当前生效配置 = 可信配置 + 开关文件覆盖（不可信的文件只能收紧）。
    pub fn effective(&mut self, base: &Config) -> Config {
        self.refresh_if_due();
        if self.cached_trusted {
            return self.cached.apply(base);
        }
        let trusted = match &self.trusted_cached {
            Some(overrides) => overrides.apply(base),
            None => base.clone(),
        };
        self.cached.tighten(&trusted)
    }

    fn refresh_if_due(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_refresh
            && now.duration_since(last) < REFRESH_EVERY
        {
            return;
        }
        self.last_refresh = Some(now);

        let existing: Vec<&PathBuf> = self.paths.iter().filter(|path| path.exists()).collect();
        let Some(first) = existing.first() else {
            // 所有路径都不存在 ⇒ 回到默认（文件被删除是"关闭覆盖"的意图）
            self.clear();
            return;
        };
        let raw = std::fs::read_to_string(first).unwrap_or_default();
        if raw.is_empty() {
            // 存在但暂时读不出来：保留上一份有效配置，等下次刷新
            return;
        }

        let is_trusted = (self.trusted)(first);
        if self.cached_raw.as_deref() == Some(raw.as_str()) {
            self.cached_trusted = is_trusted;
        } else {
            match parse(&raw) {
                Ok(overrides) => {
                    self.cached = overrides;
                    self.cached_raw = Some(raw);
                    self.cached_trusted = is_trusted;
                    self.last_error = None;
                }
                Err(err) => {
                    // 半成品/写坏的文件不能改变运行配置
                    self.report_parse_error(&err);
                    return;
                }
            }
        }

        if self.cached_trusted {
            // 可信文件可任意覆盖，没有"可信基线"一说
            self.trusted_cached = None;
            self.trusted_raw = None;
            return;
        }
        // 当前文件不可信：找可信基线——其余（更低优先级）路径里第一个存在的可信文件。
        let Some(baseline) = existing
            .iter()
            .skip(1)
            .copied()
            .find(|path| (self.trusted)(path))
        else {
            self.trusted_cached = None;
            self.trusted_raw = None;
            return;
        };
        let raw = std::fs::read_to_string(baseline).unwrap_or_default();
        if raw.is_empty() || self.trusted_raw.as_deref() == Some(raw.as_str()) {
            return;
        }
        match parse(&raw) {
            Ok(overrides) => {
                self.trusted_cached = Some(overrides);
                self.trusted_raw = Some(raw);
            }
            // 基线写坏了：保留上一份有效基线
            Err(err) => self.report_parse_error(&err),
        }
    }

    /// 同一条解析错误只报一次。
    fn report_parse_error(&mut self, err: &str) {
        if self.last_error.as_deref() != Some(err) {
            tracing::error!(event = "mem_watch.switch_invalid", error = %err);
            self.last_error = Some(err.to_string());
        }
    }

    fn clear(&mut self) {
        self.cached = Overrides::default();
        self.cached_raw = None;
        self.cached_trusted = false;
        self.trusted_cached = None;
        self.trusted_raw = None;
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

    // ── 信任边界 ──────────────────────────────────────────────────────────

    /// 测试用可信判定：文件名是 trusted.conf 的才算可信层。
    fn trust_named(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "trusted.conf")
    }

    /// 按给出顺序（优先级从高到低）写入文件并构造开关。
    fn switch_in(dir: &Path, files: &[(&str, &str)]) -> Switch {
        let mut paths = Vec::new();
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            paths.push(path);
        }
        Switch::with_trust(paths, trust_named)
    }

    #[test]
    fn stricter_goes_from_off_through_dry_to_on() {
        assert_eq!(stricter(Mode::Off, Mode::On), Mode::On);
        assert_eq!(stricter(Mode::Off, Mode::DryRun), Mode::DryRun);
        assert_eq!(stricter(Mode::DryRun, Mode::On), Mode::On);
        assert_eq!(stricter(Mode::On, Mode::Off), Mode::On);
        assert_eq!(stricter(Mode::On, Mode::DryRun), Mode::On);
        assert_eq!(stricter(Mode::DryRun, Mode::Off), Mode::DryRun);
    }

    #[test]
    fn only_the_configmap_directory_is_trusted_by_default() {
        assert!(is_trusted_path(Path::new(
            "/etc/agent-runtime/mem-watch.conf"
        )));
        assert!(!is_trusted_path(Path::new("/tmp/mem-watch.conf")));
        assert!(!is_trusted_path(Path::new(
            "/home/user/.rcoder/mem-watch.conf"
        )));
        // 按路径分量比较，不是字符串前缀
        assert!(!is_trusted_path(Path::new(
            "/etc/agent-runtime-evil/mem-watch.conf"
        )));
        assert!(!is_trusted_path(Path::new(
            "/etc/agent-runtime/../tmp/mem-watch.conf"
        )));
    }

    /// 反例（评审 L2）：/tmp 与用户家目录对容器内的业务进程可写，而它们正是
    /// 看门狗要淘汰的对象。不可信的文件不能把看门狗关掉，也不能把它调松。
    #[test]
    fn untrusted_file_cannot_weaken_the_trusted_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let mut switch = switch_in(
            dir.path(),
            &[(
                "untrusted.conf",
                "mode=off\nratio=0.95\ninterval_ms=60000\nmin_ratio=0.5\n\
                 grace_ms=99999\ncooldown_ms=999999\nbootstrap_cap_ms=999999\n",
            )],
        );
        let base = Config {
            mode: Mode::On,
            ..Config::default()
        };
        let effective = switch.effective(&base);
        assert_eq!(effective.mode, Mode::On, "不可信文件不能关掉看门狗");
        assert_eq!(effective.ratio, base.ratio);
        assert_eq!(effective.interval, base.interval);
        assert_eq!(effective.min_ratio, base.min_ratio);
        assert_eq!(effective.grace, base.grace);
        assert_eq!(effective.cooldown, base.cooldown);
        assert_eq!(effective.bootstrap_cap, base.bootstrap_cap);
    }

    #[test]
    fn untrusted_file_can_tighten() {
        let dir = tempfile::tempdir().unwrap();
        let mut switch = switch_in(
            dir.path(),
            &[("untrusted.conf", "mode=dry\nratio=0.5\ninterval_ms=250\n")],
        );
        // 基线：off / 0.85 / 500ms
        let effective = switch.effective(&Config::default());
        assert_eq!(effective.mode, Mode::DryRun);
        assert_eq!(effective.ratio, 0.5);
        assert_eq!(effective.interval, Duration::from_millis(250));
    }

    /// 灰度流程依赖这条：基线是 off 或 dry 时，按容器用不可信文件在
    /// dry ↔ on 之间来回切都行——约束是相对可信基线，不是相对当前值。
    #[test]
    fn untrusted_file_may_go_back_and_forth_above_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("untrusted.conf");
        std::fs::write(&path, "mode=on\n").unwrap();
        let mut switch = Switch::with_trust(vec![path.clone()], trust_named);
        let base = Config {
            mode: Mode::DryRun,
            ..Config::default()
        };
        assert_eq!(switch.effective(&base).mode, Mode::On);
        std::fs::write(&path, "mode=dry\n").unwrap();
        switch.last_refresh = None;
        assert_eq!(
            switch.effective(&base).mode,
            Mode::DryRun,
            "on → dry 是回滚，必须可用"
        );
        std::fs::write(&path, "mode=off\n").unwrap();
        switch.last_refresh = None;
        assert_eq!(switch.effective(&base).mode, Mode::DryRun, "不能低于基线");
    }

    /// 不可信的高优先级文件遮不住可信层：普通用户写的 /tmp 文件不能让
    /// ConfigMap 里的 mode=on 失效。
    #[test]
    fn untrusted_file_cannot_shadow_a_trusted_lower_layer() {
        let dir = tempfile::tempdir().unwrap();
        let mut switch = switch_in(
            dir.path(),
            &[
                ("untrusted.conf", "mode=off\nratio=0.99\n"),
                ("trusted.conf", "mode=on\n"),
            ],
        );
        // 环境变量：off；可信层（ConfigMap）：on；/tmp：off
        let effective = switch.effective(&Config::default());
        assert_eq!(effective.mode, Mode::On);
        assert_eq!(effective.ratio, Config::default().ratio);
    }

    #[test]
    fn trusted_file_may_weaken_and_wins_over_lower_layers() {
        let dir = tempfile::tempdir().unwrap();
        let mut switch = switch_in(
            dir.path(),
            &[
                ("trusted.conf", "mode=off\n"),
                ("untrusted.conf", "mode=on\nratio=0.5\n"),
            ],
        );
        let base = Config {
            mode: Mode::On,
            ..Config::default()
        };
        let effective = switch.effective(&base);
        assert_eq!(effective.mode, Mode::Off, "可信文件可以关掉看门狗");
        assert_eq!(
            effective.ratio, base.ratio,
            "更高优先级的文件存在时，低优先级文件不参与（既有语义）"
        );
    }

    #[test]
    fn removing_the_untrusted_file_falls_back_to_the_trusted_layer() {
        let dir = tempfile::tempdir().unwrap();
        let untrusted = dir.path().join("untrusted.conf");
        let trusted = dir.path().join("trusted.conf");
        std::fs::write(&untrusted, "mode=off\n").unwrap();
        std::fs::write(&trusted, "mode=on\nratio=0.7\n").unwrap();
        let mut switch = Switch::with_trust(vec![untrusted.clone(), trusted], trust_named);
        let base = Config::default();
        assert_eq!(switch.effective(&base).mode, Mode::On);
        std::fs::remove_file(&untrusted).unwrap();
        switch.last_refresh = None;
        let effective = switch.effective(&base);
        assert_eq!(effective.mode, Mode::On);
        assert_eq!(effective.ratio, 0.7, "只剩可信文件时它可任意覆盖");
    }
}

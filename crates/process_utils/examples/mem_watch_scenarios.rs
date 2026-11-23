//! 场景化端到端验证：文件开关的信任边界、启动窗口保护、dry-run、紧急止损。
//!
//! 在受限 cgroup 中运行（`--memory=512m`，容器内以 root 运行）。可信目录是
//! 只读挂载时容器内写不了，所以由宿主侧驱动
//! `crates/process_utils/examples/mem_watch_scenarios.sh` 起容器，并按本程序打印的
//! `SCN-SET trusted=<off|dry|absent>` 行改写挂载源（模拟运维改 ConfigMap）。
//!
//! * `unmounted`——可信目录没有只读挂载：容器内进程自己 `mkdir -p` 出来的
//!   "可信文件"（`mode=off`）不生效，hog 照常被淘汰。
//! * `mounted`——可信目录是宿主侧目录的只读挂载：
//!   1. **可信层 off** → hog 不被杀；
//!   2. **不可信的 /tmp 关不掉看门狗**：移除可信文件（回落到传入的默认
//!      `On`），同时在 `/tmp/mem-watch.conf` 写 `mode=off` → 新 hog 照常被杀，
//!      而**封窗前**启动的 hog 因启动窗口保护而存活；
//!   3. **可信层 dry** → 超阈值但只记录，hog 存活；
//!   4. **紧急止损**：`/tmp` 写 `mode=on` 之后可信层写 `mode=off` → 以可信层
//!      为准，hog 存活。
//!
//! 两个模式都要求全程 agent 根进程（本进程）存活，容器不被 OOM。
//!
//! 开关文件的信任规则见 `mem_watch::switch` 的模块文档：只有可信目录下、位于
//! 只读挂载上的文件能任意覆盖，其余文件只能把配置调得更严。
//!
//! 限额从**本容器 cgroup 实测**（不用 env 覆盖伪造）：看门狗要求用量与限额
//! 同源，伪造值和实测 `memory.max` 不一致时它会拒绝淘汰。因此本示例按实测
//! 限额缩放 hog 体积，并要求限额落在可安全测试的区间内。

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure};
use process_utils::mem_watch::{
    self, Config, Mode,
    cgroup::CgroupProbe,
    registry,
    switch::{DEFAULT_PATHS, is_trusted_path},
};

/// 不可信的开关文件：容器内任何进程都能写。
const UNTRUSTED_CONF: &str = "/tmp/mem-watch.conf";
/// 可信的开关文件：ConfigMap 的只读挂载点，只有运维能写。
const TRUSTED_CONF: &str = "/etc/agent-runtime/mem-watch.conf";
/// 可安全测试的限额区间：下界保证小 hog 能超过 min_rss 下限，
/// 上界保证总分配不超过限额的 ~75%。
const MIN_TESTABLE_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_TESTABLE_LIMIT: u64 = 1024 * 1024 * 1024;
/// 开关刷新节流是 1s；等过它再下断言，避免"配置还没生效"被误判成通过。
const SETTLE: Duration = Duration::from_millis(1500);
/// 一次淘汰观测窗：grace 1s + 采样/冷却的余量。
const KILL_WINDOW: Duration = Duration::from_secs(7);

#[derive(Clone, Copy)]
enum Scenario {
    Unmounted,
    Mounted,
}

fn main() -> anyhow::Result<()> {
    // RUST_LOG 非空时输出看门狗内部事件（无订阅者时 tracing 全部静默，
    // 场景断言只看进程状态；需要排查时设 RUST_LOG=info 可看到全部）。
    if std::env::var("RUST_LOG").is_ok() {
        drop(tracing_subscriber::fmt::try_init());
    }
    let mut args = std::env::args().skip(1);
    let scenario = match (args.next().as_deref(), args.next()) {
        (Some("unmounted"), None) => Scenario::Unmounted,
        (Some("mounted"), None) => Scenario::Mounted,
        _ => bail!("usage: mem_watch_scenarios <unmounted|mounted>"),
    };

    let stage = Stage::start()?;
    match scenario {
        Scenario::Unmounted => stage.unmounted()?,
        Scenario::Mounted => stage.mounted()?,
    }
    println!("ALL SCENARIOS PASS");
    Ok(())
}

struct Stage {
    /// 承载采样任务的 runtime 与任务句柄。必须活到场景结束：runtime 一丢弃，
    /// 采样任务就在下一个 await 点被取消——场景会"看起来在跑"，实际再也没有
    /// 淘汰发生。
    _runtime: tokio::runtime::Runtime,
    _watcher: tokio::task::JoinHandle<()>,
    hog_bin: PathBuf,
    /// 单进程即超阈值（55% 限额）。
    big_hog_mb: usize,
    /// 自己不超阈值（18% 限额），要与大 hog 同时在才把总量顶过阈值。
    small_hog_mb: usize,
}

impl Stage {
    fn start() -> anyhow::Result<Self> {
        let registry = registry::install();
        let self_pid = std::process::id();
        registry.register("scn", self_pid).ok_or_else(|| {
            anyhow::anyhow!("cannot register self pid {self_pid}: /proc unreadable")
        })?;
        println!("[scn] root pid={self_pid}");

        // 限额与用量必须同源：用实测值，让看门狗的口径校验自然通过。
        let probe = CgroupProbe::resolve();
        let limit = probe.max().filter(|v| {
            (MIN_TESTABLE_LIMIT..=MAX_TESTABLE_LIMIT).contains(v) && probe.relpath().is_empty()
        });
        let Some(limit) = limit else {
            bail!(
                "限额不可用或超出可测区间: cgroup_max={:?} relpath={:?}，要求 {}..={} 且 relpath 为空；\
                 请在带内存限额的容器里运行，例如 docker run --rm --memory=512m ...",
                probe.max(),
                probe.relpath(),
                MIN_TESTABLE_LIMIT,
                MAX_TESTABLE_LIMIT
            );
        };
        let limit_mb = limit / (1024 * 1024);
        let big_hog_mb = (limit_mb * 55 / 100) as usize;
        let small_hog_mb = ((limit_mb * 18 / 100) as usize).max(72);
        println!(
            "[scn] 实测限额 {limit_mb}MB → big hog={big_hog_mb}MB small hog={small_hog_mb}MB（阈值 40% = {}MB）",
            limit_mb * 40 / 100
        );

        let exe = std::env::current_exe()?;
        let hog_bin = exe.parent().expect("exe dir").join("mem_hog");
        ensure!(
            hog_bin.exists(),
            "先 cargo build --examples: {}",
            hog_bin.display()
        );

        // 自检：本示例依赖两个路径的信任分类，分类变了场景就会测错东西。
        ensure!(
            DEFAULT_PATHS.contains(&TRUSTED_CONF) && DEFAULT_PATHS.contains(&UNTRUSTED_CONF),
            "默认查找路径变了，示例里的两个路径不再是其中之一"
        );
        ensure!(
            !is_trusted_path(Path::new(UNTRUSTED_CONF)),
            "不可信路径的判定变了：{UNTRUSTED_CONF} 被视为可信层"
        );

        // 传入默认 On（可信基线）。场景里的可信层文件把它改掉。
        let cfg = Config {
            mode: Mode::On,
            ratio: 0.4, // 阈值 = 40% 实测限额
            interval: Duration::from_millis(100),
            min_ratio: 0.01,
            grace: Duration::from_secs(1),
            cooldown: Duration::from_millis(500),
            bootstrap_cap: Duration::from_secs(120),
            ..Default::default()
        };
        // mem_watch::run 需要 tokio 上下文：起一个后台 runtime 承载采样任务。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let watcher = rt.spawn(mem_watch::run(cfg));

        Ok(Self {
            _runtime: rt,
            _watcher: watcher,
            hog_bin,
            big_hog_mb,
            small_hog_mb,
        })
    }

    /// 可信目录没有只读挂载：容器内进程写出的"可信文件"只能收紧，关不掉看门狗。
    fn unmounted(&self) -> anyhow::Result<()> {
        let trusted = Path::new(TRUSTED_CONF);
        ensure!(!trusted.exists(), "本场景要求 {TRUSTED_CONF} 一开始不存在");
        // 模拟容器内的业务进程：按路径名伪造一个"可信文件"。
        std::fs::create_dir_all(trusted.parent().expect("parent"))?;
        std::fs::write(trusted, "mode=off\n")?;
        ensure!(
            !is_trusted_path(trusted),
            "伪造文件被当成了可信层：{TRUSTED_CONF}（驱动不应把该目录只读挂进来）"
        );
        std::thread::sleep(SETTLE);

        // 封窗后起 hog：不受启动窗口保护，看门狗若仍开着就必须淘汰它。
        mem_watch::mark_session_active("scn");
        println!("[scn-u] 伪造可信路径文件 mode=off（不在只读挂载上）; seal; spawn hogU");
        let mut hog_u = self.spawn(self.big_hog_mb)?;
        std::thread::sleep(KILL_WINDOW);
        hog_u.must_be_dead("hogU")?;
        println!("[scn-u] PASS hogU 被淘汰（伪造的可信文件没能关掉看门狗）");
        Ok(())
    }

    /// 可信目录是宿主侧目录的只读挂载：按 SCN-SET 行让驱动改写它。
    fn mounted(&self) -> anyhow::Result<()> {
        ensure!(
            is_trusted_path(Path::new(TRUSTED_CONF)),
            "可信路径的判定变了，或驱动没有把目录只读挂到 {}",
            Path::new(TRUSTED_CONF).parent().expect("parent").display()
        );
        self.await_trusted(None)?;

        // ── 场景 1：可信层 off ─────────────────────────────────────────
        println!("SCN-SET trusted=off");
        self.await_trusted(Some("mode=off\n"))?;
        println!(
            "[scn1] 可信层 mode=off, spawn hogA({}MB，单进程即超阈值)",
            self.big_hog_mb
        );
        let mut hog_a = self.spawn(self.big_hog_mb)?;
        std::thread::sleep(Duration::from_secs(4));
        hog_a.must_be_alive("hogA")?;
        println!("[scn1] PASS hogA 存活（可信层 off，不杀）");

        // ── 场景 2：不可信的 /tmp 关不掉看门狗 + 启动窗口保护 ──────────
        // 移除可信文件 → 回落到默认 On。同时在 /tmp 写 mode=off：/tmp 是不可信
        // 层，关不掉基线为 On 的看门狗。hogA 在封窗前启动，会被启动窗口快照
        // 保护；此时封窗（模拟首条用户消息），再启动 hogB → 应被淘汰。
        println!("SCN-SET trusted=absent");
        self.await_trusted(None)?;
        std::fs::write(UNTRUSTED_CONF, "mode=off\n")?;
        std::thread::sleep(SETTLE);
        println!("[scn2] 可信文件已移除 → 默认 On；/tmp 写 mode=off（不可信，不生效）; seal");
        mem_watch::mark_session_active("scn");
        println!("[scn2] sealed, spawn hogB");
        let mut hog_b = self.spawn(self.small_hog_mb)?;
        std::thread::sleep(KILL_WINDOW);
        hog_a.must_be_alive("hogA")?;
        hog_b.must_be_dead("hogB")?;
        println!("[scn2] PASS hogB 被淘汰（/tmp 的 mode=off 没能关掉看门狗），hogA（窗口内）存活");

        // ── 场景 3：可信层 dry ─────────────────────────────────────────
        std::fs::remove_file(UNTRUSTED_CONF)?;
        println!("SCN-SET trusted=dry");
        self.await_trusted(Some("mode=dry\n"))?;
        println!("[scn3] 可信层 mode=dry, spawn hogC");
        let mut hog_c = self.spawn(self.small_hog_mb)?;
        std::thread::sleep(Duration::from_secs(5));
        hog_c.must_be_alive("hogC")?;
        println!("[scn3] PASS hogC 存活（dry 只记录）");
        // 场景 4 会有一个看门狗实际开着的窗口（见下），先收掉不受保护的大 hog。
        hog_c.reap()?;

        // ── 场景 4：紧急止损——可信层压过 /tmp 调严的 on ────────────────
        // /tmp 写 mode=on 相当于把 dry 调严成 on（灰度容器的形态）；在可信层
        // 变成 off 之前，看门狗真的开着——此时只剩受启动窗口保护的 hogA。
        std::fs::write(UNTRUSTED_CONF, "mode=on\n")?;
        println!("SCN-SET trusted=off");
        self.await_trusted(Some("mode=off\n"))?;
        println!("[scn4] /tmp mode=on（不可信，只能收紧）+ 可信层 mode=off; spawn hogD");
        let mut hog_d = self.spawn(self.big_hog_mb)?;
        std::thread::sleep(KILL_WINDOW);
        hog_d.must_be_alive("hogD")?;
        println!("[scn4] PASS hogD 存活（可信层的 off 压过了 /tmp 的 on）");
        hog_d.reap()?;
        hog_a.reap()?;
        Ok(())
    }

    /// 等驱动把可信文件改成期望内容（模拟 kubelet 同步 ConfigMap），再等开关刷新。
    fn await_trusted(&self, expected: Option<&str>) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let current = std::fs::read_to_string(TRUSTED_CONF).ok();
            if current.as_deref() == expected {
                break;
            }
            if Instant::now() > deadline {
                bail!("可信文件没有变成 {expected:?}（当前 {current:?}）——驱动没有响应 SCN-SET？");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(SETTLE);
        Ok(())
    }

    fn spawn(&self, mb: usize) -> anyhow::Result<Hog> {
        let child = std::process::Command::new(&self.hog_bin)
            .arg(mb.to_string())
            .spawn()?;
        println!("[scn] spawned pid={} ({mb}MB)", child.id());
        Ok(Hog(child))
    }
}

/// 一只 hog 进程。离开作用域时一定被收掉——场景中途断言失败（`?` 提前返回）
/// 也不留给后面的场景。
struct Hog(Child);

impl Hog {
    fn must_be_alive(&mut self, name: &str) -> anyhow::Result<()> {
        match self.0.try_wait()? {
            Some(status) => bail!("{name} 提前退出（{status}），不符合预期"),
            None => Ok(()),
        }
    }

    fn must_be_dead(&mut self, name: &str) -> anyhow::Result<()> {
        match self.0.try_wait()? {
            Some(_) => Ok(()),
            None => bail!("{name} 仍然存活，看门狗没有触发"),
        }
    }

    fn reap(&mut self) -> anyhow::Result<()> {
        self.0.kill()?;
        self.0.wait()?;
        Ok(())
    }
}

impl Drop for Hog {
    fn drop(&mut self) {
        // 已退出的进程 kill/wait 都是空操作。
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

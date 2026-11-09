//! 场景化端到端验证：文件开关、启动窗口保护、dry-run。
//!
//! 在受限 cgroup 中运行（建议 `--memory=512m`），覆盖四个场景：
//!
//! 1. **off**：`/tmp/mem-watch.conf` 写 `mode=off` → hog 不被杀；
//! 2. **on**：删除开关文件（回落到传入的默认 `On`）→ 新 hog 被杀，
//!    而**封窗前**启动的 hog 因启动窗口保护而存活；
//! 3. **dry**：写 `mode=dry` → 超阈值但只记录，hog 存活；
//! 4. 全程 agent 根进程（本进程）存活，容器不被 OOM。
//!
//! ```bash
//! docker run --rm --memory=512m \
//!   -v "$PWD:/w" -v "$HOME/.cargo/registry:/cargo/registry" \
//!   -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/t -w /w rust:1 \
//!   sh -c "cargo build -p process_utils --examples && \
//!          /t/debug/examples/mem_watch_scenarios"
//! ```

use std::process::Child;
use std::time::Duration;

use process_utils::mem_watch::{self, registry, Config, Mode};

const CONF: &str = "/tmp/mem-watch.conf";
/// 场景 1 用超大 hog：只有它一个就超过 40% 阈值，
/// 这样「off 不杀」才是真验证（否则只是内存没到阈值）。
const HOG_A_MB: usize = 250;
const HOG_MB: usize = 150;

fn main() -> anyhow::Result<()> {
    let registry = registry::install();
    let self_pid = std::process::id();
    registry.register("scn", self_pid).expect("register self");
    println!("[scn] root pid={self_pid}");

    let exe = std::env::current_exe()?;
    let hog_bin = exe.parent().expect("exe dir").join("mem_hog");
    anyhow::ensure!(hog_bin.exists(), "先 cargo build --examples: {}", hog_bin.display());

    // 传入默认 On；场景 1 用文件覆盖成 off。
    let cfg = Config {
        mode: Mode::On,
        ratio: 0.4,                       // 512MiB → 204MiB 触发
        interval: Duration::from_millis(100),
        min_ratio: 0.01,
        grace: Duration::from_secs(1),
        cooldown: Duration::from_millis(500),
        bootstrap_cap: Duration::from_secs(120),
        ..Default::default()
    };
    // mem_watch::start 需要 tokio 上下文：起一个后台 runtime 承载采样任务。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let _watcher = rt.spawn(mem_watch::run(cfg));

    // ── 场景 1：off ────────────────────────────────────────────────
    std::fs::write(CONF, "mode=off\n")?;
    // 等开关生效（刷新节流 1s）：确保 spawn 时配置已经是 off，
    // 否则场景 1 通过可能只是"内存没到阈值"，而不是"off 生效"。
    std::thread::sleep(Duration::from_millis(1500));
    println!("[scn1] mode=off, spawn hogA({HOG_A_MB}MB，单进程即超阈值)");
    let mut hog_a = spawn(&hog_bin, HOG_A_MB)?;
    std::thread::sleep(Duration::from_secs(4));
    assert_child_alive(&mut hog_a, "hogA")?;
    println!("[scn1] PASS hogA 存活（off 不杀）");

    // ── 场景 2：on + 启动窗口保护 ──────────────────────────────────
    // 删除开关文件 → 回落到默认 On。hogA 在封窗前启动，会被启动窗口
    // 快照保护；此时封窗（模拟首条用户消息），再启动 hogB → 应被淘汰。
    std::fs::remove_file(CONF)?;
    println!("[scn2] conf removed → default On; seal（封窗，hogA 受保护）");
    std::thread::sleep(Duration::from_millis(400)); // 让 off 状态结束、快照吸收 hogA
    mem_watch::mark_session_active("scn");
    println!("[scn2] sealed, spawn hogB");
    let mut hog_b = spawn(&hog_bin, HOG_MB)?;
    std::thread::sleep(Duration::from_secs(7));
    assert_child_alive(&mut hog_a, "hogA")?;
    assert_child_dead(&mut hog_b, "hogB")?;
    println!("[scn2] PASS hogB 被淘汰，hogA（窗口内）存活");

    // ── 场景 3：dry ────────────────────────────────────────────────
    std::fs::write(CONF, "mode=dry\n")?;
    // 等旧配置（on）过期，避免 hogC 在残留的 on 窗口内被杀。
    std::thread::sleep(Duration::from_millis(1500));
    println!("[scn3] mode=dry, spawn hogC");
    let mut hog_c = spawn(&hog_bin, HOG_MB)?;
    std::thread::sleep(Duration::from_secs(5));
    assert_child_alive(&mut hog_c, "hogC")?;
    println!("[scn3] PASS hogC 存活（dry 只记录）");

    // ── 清理 ───────────────────────────────────────────────────────
    drop(std::fs::remove_file(CONF));
    for child in [&mut hog_a, &mut hog_c] {
        drop(child.kill());
        drop(child.wait());
    }
    println!("ALL SCENARIOS PASS");
    Ok(())
}

fn spawn(hog_bin: &std::path::Path, mb: usize) -> anyhow::Result<Child> {
    let child = std::process::Command::new(hog_bin).arg(mb.to_string()).spawn()?;
    println!("[scn] spawned pid={} ({mb}MB)", child.id());
    Ok(child)
}

fn assert_child_alive(child: &mut Child, name: &str) -> anyhow::Result<()> {
    match child.try_wait()? {
        Some(status) => anyhow::bail!("{name} 提前退出（{status}），不符合预期"),
        None => Ok(()),
    }
}

fn assert_child_dead(child: &mut Child, name: &str) -> anyhow::Result<()> {
    match child.try_wait()? {
        Some(_) => Ok(()),
        None => {
            drop(child.kill());
            drop(child.wait());
            anyhow::bail!("{name} 仍然存活，看门狗没有触发")
        }
    }
}

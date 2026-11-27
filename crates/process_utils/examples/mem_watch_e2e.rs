//! 端到端验证：在受限 cgroup 里跑 3 个 hog，看门狗应杀掉至少 1 个，
//! 而"agent 根进程"（本进程）必须存活。
//!
//! 运行方式（必须有内存上限，否则不会触发）：
//! ```bash
//! docker run --rm --memory=512m -v "$PWD:/w" -w /w rust:1 \
//!   sh -c "cargo build -p process_utils --examples && ./target/debug/examples/mem_watch_e2e"
//! ```

use std::time::Duration;

use process_utils::mem_watch::{self, Config, Mode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = mem_watch::registry::install();
    let self_pid = std::process::id();
    let Some(root) = registry.register("e2e", self_pid) else {
        anyhow::bail!("register 失败：读不到 /proc/{self_pid}");
    };
    println!("root pid={} exe={:?}", root.pid, root.exe);

    // 模拟"第一条用户消息"：封窗。此后 spawn 的进程才算命令。
    mem_watch::mark_session_active("e2e");
    println!(
        "sealed={}",
        registry.get("e2e").is_some_and(|r| r.is_sealed())
    );

    let exe = std::env::current_exe()?;
    let hog = exe.parent().expect("exe dir").join("mem_hog");
    anyhow::ensure!(
        hog.exists(),
        "找不到 {}，先 cargo build --examples",
        hog.display()
    );

    let mut children = Vec::new();
    for i in 0..3 {
        let child = std::process::Command::new(&hog).arg("150").spawn()?;
        println!("spawned hog {i} pid={}", child.id());
        children.push(child);
    }

    let cfg = Config {
        mode: Mode::On,
        ratio: 0.4,
        interval: Duration::from_millis(100),
        min_ratio: 0.01,
        grace: Duration::from_secs(1),
        cooldown: Duration::from_millis(500),
        bootstrap_cap: Duration::from_secs(60),
        ..Default::default()
    };
    let _watcher = mem_watch::start(cfg);

    tokio::time::sleep(Duration::from_secs(6)).await;

    let (mut alive, mut dead) = (0, 0);
    for mut child in children {
        match child.try_wait()? {
            Some(_) => dead += 1,
            None => {
                alive += 1;
                drop(child.kill());
            }
        }
    }
    println!("RESULT alive={alive} dead={dead}");
    anyhow::ensure!(dead >= 1, "看门狗没有杀掉任何 hog");
    anyhow::ensure!(alive >= 1, "看门狗把 hog 全杀光了（应只杀 1 个）");
    println!("OK");
    Ok(())
}

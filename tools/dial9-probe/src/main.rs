//! dial9 trace 证据探针（observability 验收用，非业务代码）。
//!
//! 离线解码 trace 段目录（`trace.<index>.bin`），输出三类证据并按断言退出：
//! 1. instrumented spawn：`TaskSpawnEvent.instrumented == true`（经 rcoder-obs
//!    门面 / dial9 spawner 的任务）；打印 task_id、spawn_loc、worker。
//! 2. wake 配对：`WakeEvent.woken_task_id` 命中 instrumented 任务；唤醒者
//!    `waker_task_id` 无 Tokio 任务上下文时为 UNKNOWN（合法，不判失败）。
//! 3. task dump：`TaskDumpEvent`（录制端开 DIAL9_TASK_DUMP_ENABLED 才有）。
//!
//! 退出码：断言不满足 = 非零（fail fast）；仅打印模式断言全给默认值时=0。
//!
//! 用法：
//! ```text
//! dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>...
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dial9::analysis::analysis_events::{Dial9Event, WorkerId};
use dial9::analysis::TraceReader;
use dial9::analysis::analysis_events::WakeEvent;

#[derive(Debug, Default)]
struct Evidence {
    files: usize,
    events_total: usize,
    spawns_total: usize,
    /// instrumented=true 的 spawn：task_id → (spawn_loc, worker)
    instrumented: HashMap<u64, (String, Option<u64>)>,
    /// woken 侧命中 instrumented 任务的 wake 事件数（含 waker UNKNOWN 计数）
    wake_pairs_to_instrumented: usize,
    wake_pairs_unknown_waker: usize,
    wakes_total: usize,
    dumps: usize,
    dump_tasks: std::collections::BTreeSet<u64>,
}

fn main() -> Result<()> {
    let mut min_instrumented = 1usize;
    let mut expect_wake_pairs = false;
    let mut expect_dump = false;
    let mut paths: Vec<PathBuf> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--min-instrumented" => {
                let value = args.next().context("--min-instrumented 需要参数")?;
                min_instrumented = value.parse().context("min-instrumented 不是数字")?;
            }
            "--expect-wake-pairs" => expect_wake_pairs = true,
            "--expect-dump" => expect_dump = true,
            "--" => {}
            "-h" | "--help" => {
                println!("dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>...");
                println!("  --min-instrumented N   断言 instrumented spawn 数 >= N（默认 1）");
                println!("  --expect-wake-pairs    断言存在 woken=instrumented 任务的 wake 事件");
                println!("  --expect-dump          断言存在 TaskDumpEvent（录制端需 DIAL9_TASK_DUMP_ENABLED=1）");
                return Ok(());
            }
            other if other.starts_with("--") => {
                bail!("未知参数: {other}")
            }
            other => paths.push(PathBuf::from(other)),
        }
    }
    if paths.is_empty() {
        bail!("用法: dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>...");
    }

    let mut files = Vec::new();
    for path in &paths {
        collect_trace_files(path, &mut files)?;
    }
    if files.is_empty() {
        bail!("未找到 trace 段文件（命名 trace.<index>.bin）: {paths:?}");
    }

    let mut evidence = Evidence::default();
    for file in &files {
        let reader = TraceReader::new(
            file.to_str().with_context(|| format!("路径非 UTF-8: {file:?}"))?,
        )
        .with_context(|| format!("解码失败: {file:?}"))?;
        evidence.files += 1;
        for event in &reader.all_events {
            evidence.events_total += 1;
            match event {
                Dial9Event::TaskSpawnEvent(spawn) => {
                    evidence.spawns_total += 1;
                    if spawn.instrumented {
                        evidence.instrumented.insert(
                            spawn.task_id,
                            (spawn.spawn_loc.clone(), spawn.worker_id.map(WorkerId::as_u64)),
                        );
                    }
                }
                Dial9Event::WakeEvent(wake) => {
                    evidence.wakes_total += 1;
                    if evidence.instrumented.contains_key(&wake.woken_task_id) {
                        evidence.wake_pairs_to_instrumented += 1;
                        if is_unknown_waker(wake) {
                            evidence.wake_pairs_unknown_waker += 1;
                        }
                    }
                }
                Dial9Event::TaskDumpEvent(dump) => {
                    evidence.dumps += 1;
                    evidence.dump_tasks.insert(dump.task_id);
                }
                _ => {}
            }
        }
    }

    println!("files decoded           : {}", evidence.files);
    println!("events total            : {}", evidence.events_total);
    println!("task spawns (all)       : {}", evidence.spawns_total);
    println!("instrumented spawns     : {}", evidence.instrumented.len());
    // 按 spawn 位置聚合（各接入点的真实事件量），再抽样列前几个任务
    let mut by_loc: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (_task_id, (loc, _worker)) in evidence.instrumented.iter() {
        *by_loc.entry(loc.as_str()).or_default() += 1;
    }
    for (loc, count) in &by_loc {
        println!("  instrumented @ {loc} × {count}");
    }
    for (task_id, (loc, worker)) in evidence.instrumented.iter().take(5) {
        println!("  e.g. task {task_id} worker={worker:?} loc={loc}");
    }
    println!("wake events (all)       : {}", evidence.wakes_total);
    println!(
        "wake→instrumented pairs : {} (其中 waker=UNKNOWN {})",
        evidence.wake_pairs_to_instrumented, evidence.wake_pairs_unknown_waker
    );
    println!("task dumps              : {} on {} tasks", evidence.dumps, evidence.dump_tasks.len());
    for task_id in evidence.dump_tasks.iter().take(10) {
        let instrumented = if evidence.instrumented.contains_key(task_id) { "instrumented" } else { "uninstrumented" };
        println!("  dump for task {task_id} ({instrumented})");
    }

    let mut failures = Vec::new();
    if evidence.instrumented.len() < min_instrumented {
        failures.push(format!(
            "instrumented spawns {} < 要求 {}",
            evidence.instrumented.len(),
            min_instrumented
        ));
    }
    if expect_wake_pairs && evidence.wake_pairs_to_instrumented == 0 {
        failures.push("要求 wake 配对（woken=instrumented 任务）但一条也没有".into());
    }
    if expect_dump && evidence.dumps == 0 {
        failures.push("要求 task dump 事件但一条也没有".into());
    }
    if !failures.is_empty() {
        for failure in failures {
            eprintln!("ASSERTION FAILED: {failure}");
        }
        std::process::exit(1);
    }
    println!("OK: 断言全部满足");
    Ok(())
}

/// 唤醒发生在无 Tokio 任务上下文（线程直接 unpark / 外部来源）时 waker 为
/// UNKNOWN（合法形态，见 specs T3.4 措辞），仅统计不判失败。
fn is_unknown_waker(wake: &WakeEvent) -> bool {
    wake.waker_task_id == 0
}

/// 递归收集 trace 段（dial9 命名空间形态：`<dir>/<namespace>/trace.<index>.bin`，
/// namespace 为 boot/主机维度子目录；也兼容直接落 <dir>/trace.<index>.bin）。
fn collect_trace_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("访问失败: {path:?}"))?;
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(path).with_context(|| format!("读目录失败: {path:?}"))? {
            let entry = entry.with_context(|| format!("读目录项失败: {path:?}"))?;
            let child = entry.path();
            if child.is_dir() {
                collect_trace_files(&child, out)?;
            } else if child
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("trace.") && n.ends_with(".bin"))
            {
                entries.push(child);
            }
        }
        entries.sort();
        out.extend(entries);
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

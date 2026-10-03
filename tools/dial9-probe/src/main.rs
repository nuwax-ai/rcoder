//! dial9 trace 证据探针（observability 验收用，非业务代码）。
//!
//! 离线解码 trace 段目录（`trace.<index>.bin`），输出三类证据并按断言退出：
//! 1. instrumented spawn：`TaskSpawnEvent.instrumented == true`（经 rcoder-obs
//!    门面 / dial9 spawner 的任务）；打印 task_id、spawn_loc、worker。
//! 2. wake 配对：`WakeEvent.woken_task_id` 命中 instrumented 任务；唤醒者
//!    `waker_task_id` 无 Tokio 任务上下文时为 UNKNOWN（合法，不判失败）。
//! 3. task dump：`TaskDumpEvent`（录制端开 DIAL9_TASK_DUMP_ENABLED 才有）。
//!
//! 退出码：断言不满足 = 非零；仅打印时传 --min-instrumented 0 且不启用期望标志。
//! TaskId 仅在同一录制内有意义，跨目录不关联；完整分析后再配对，不依赖段顺序。
//! 保留录制的原 namespace 目录；不支持将不同录制段平铺到同一目录，
//! 本工具按目录隔离，未核验重打包后的内在录制身份。
//!
//! 用法：
//! ```text
//! dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>...
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dial9::analysis::TraceReader;
use dial9::analysis::analysis_events::WakeEvent;
use dial9::analysis::analysis_events::{Dial9Event, WorkerId};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct TaskKey {
    recording: PathBuf,
    id: u64,
}

#[derive(Debug, Default)]
struct Evidence {
    files: usize,
    events_total: usize,
    spawns_total: usize,
    instrumented: HashMap<TaskKey, (String, Option<u64>)>,
    // Store targets first; their spawn may be in a later supplied segment.
    wakes: HashMap<TaskKey, (usize, usize)>,
    wakes_total: usize,
    dumps: usize,
    dump_tasks: BTreeSet<TaskKey>,
}

impl Evidence {
    fn ingest(&mut self, recording: &Path, events: &[Dial9Event]) {
        for event in events {
            self.events_total += 1;
            let key = |id| TaskKey {
                recording: recording.to_path_buf(),
                id,
            };
            match event {
                Dial9Event::TaskSpawnEvent(spawn) => {
                    self.spawns_total += 1;
                    if spawn.instrumented {
                        self.instrumented.insert(
                            key(spawn.task_id),
                            (
                                spawn.spawn_loc.clone(),
                                spawn.worker_id.map(WorkerId::as_u64),
                            ),
                        );
                    }
                }
                Dial9Event::WakeEvent(wake) => {
                    self.wakes_total += 1;
                    let counts = self.wakes.entry(key(wake.woken_task_id)).or_default();
                    counts.0 += 1;
                    counts.1 += usize::from(is_unknown_waker(wake));
                }
                Dial9Event::TaskDumpEvent(dump) => {
                    self.dumps += 1;
                    self.dump_tasks.insert(key(dump.task_id));
                }
                _ => {}
            }
        }
    }

    fn wake_pairs(&self) -> (usize, usize) {
        self.wakes
            .iter()
            .filter(|(key, _)| self.instrumented.contains_key(*key))
            .fold((0, 0), |sum, (_, counts)| {
                (sum.0 + counts.0, sum.1 + counts.1)
            })
    }

    fn failures(&self, minimum: usize, expect_wake: bool, expect_dump: bool) -> Vec<String> {
        let mut failures = Vec::new();
        if self.instrumented.len() < minimum {
            failures.push(format!(
                "instrumented spawns {} < 要求 {}",
                self.instrumented.len(),
                minimum
            ));
        }
        if expect_wake && self.wake_pairs().0 == 0 {
            failures.push("要求同一录制的 wake→instrumented 配对，但没有观察到".into());
        }
        if expect_dump {
            if self.dumps == 0 {
                failures.push("要求 task dump 事件但一条也没有".into());
            }
            let missing = self
                .dump_tasks
                .iter()
                .filter(|key| !self.instrumented.contains_key(*key))
                .count();
            if missing > 0 {
                failures.push(format!(
                    "{missing} 个 dump 任务缺少同一录制的 instrumented spawn，可能缺少早期段"
                ));
            }
        }
        failures
    }
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
            "--" => {
                paths.extend(args.map(PathBuf::from));
                break;
            }
            "-h" | "--help" => {
                println!(
                    "dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>..."
                );
                println!("  --min-instrumented N   断言 instrumented spawn 数 >= N（默认 1）");
                println!("  --expect-wake-pairs    断言存在 woken=instrumented 任务的 wake 事件");
                println!(
                    "  --expect-dump          断言存在 TaskDumpEvent（录制端需 DIAL9_TASK_DUMP_ENABLED=1）"
                );
                return Ok(());
            }
            other if other.starts_with("--") => {
                bail!("未知参数: {other}")
            }
            other => paths.push(PathBuf::from(other)),
        }
    }
    if paths.is_empty() {
        bail!(
            "用法: dial9-probe [--min-instrumented N] [--expect-wake-pairs] [--expect-dump] <trace-dir|trace-file>..."
        );
    }

    let mut files = BTreeSet::new();
    let mut visited = HashSet::new();
    for path in &paths {
        collect_trace_files(path, &mut files, &mut visited)?;
    }
    if files.is_empty() {
        bail!("未找到 trace 段文件（命名 trace.<index>.bin）: {paths:?}");
    }

    let mut evidence = Evidence::default();
    for file in &files {
        let reader = TraceReader::new(
            file.to_str()
                .with_context(|| format!("路径非 UTF-8: {file:?}"))?,
        )
        .with_context(|| format!("解码失败: {file:?}"))?;
        evidence.files += 1;
        // Canonical paths keep directory/file aliases in one recording.
        let recording = file.parent().context("trace 文件缺少录制目录")?;
        evidence.ingest(recording, &reader.all_events);
    }
    let (wake_pairs, unknown_pairs) = evidence.wake_pairs();

    println!("files decoded           : {}", evidence.files);
    println!("events total            : {}", evidence.events_total);
    println!("task spawns (all)       : {}", evidence.spawns_total);
    println!("instrumented spawns     : {}", evidence.instrumented.len());
    // 按 spawn 位置聚合（各接入点的真实事件量），再抽样列前几个任务
    let mut by_loc: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (loc, _worker) in evidence.instrumented.values() {
        *by_loc.entry(loc.as_str()).or_default() += 1;
    }
    for (loc, count) in &by_loc {
        println!("  instrumented @ {loc} × {count}");
    }
    for (key, (loc, worker)) in evidence.instrumented.iter().take(5) {
        println!(
            "  e.g. recording={} task={} worker={worker:?} loc={loc}",
            key.recording.display(),
            key.id
        );
    }
    println!("wake events (all)       : {}", evidence.wakes_total);
    println!(
        "wake→instrumented pairs : {} (其中 waker=UNKNOWN {})",
        wake_pairs, unknown_pairs
    );
    println!(
        "task dumps              : {} on {} tasks",
        evidence.dumps,
        evidence.dump_tasks.len()
    );
    for key in evidence.dump_tasks.iter().take(10) {
        let instrumented = if evidence.instrumented.contains_key(key) {
            "instrumented"
        } else {
            "unmatched"
        };
        println!(
            "  dump recording={} task={} ({instrumented})",
            key.recording.display(),
            key.id
        );
    }

    let failures = evidence.failures(min_instrumented, expect_wake_pairs, expect_dump);
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
fn collect_trace_files(
    path: &Path,
    out: &mut BTreeSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("访问失败: {path:?}"))?;
    let meta =
        std::fs::metadata(&canonical).with_context(|| format!("读取元数据失败: {path:?}"))?;
    if meta.is_dir() {
        if !visited.insert(canonical.clone()) {
            return Ok(());
        }
        for entry in
            std::fs::read_dir(&canonical).with_context(|| format!("读目录失败: {path:?}"))?
        {
            let entry = entry.with_context(|| format!("读目录项失败: {path:?}"))?;
            let child = entry.path();
            if child.is_dir()
                || child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("trace.") && n.ends_with(".bin"))
            {
                collect_trace_files(&child, out, visited)?;
            }
        }
    } else if meta.is_file() {
        out.insert(canonical);
    } else {
        bail!("trace 输入不是文件或目录: {path:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, id: u64) -> Dial9Event {
        let value = match kind {
            "spawn" => serde_json::json!({"event":"TaskSpawnEvent", "timestamp_ns":1, "task_id":id,
                "spawn_loc":"fixture:1", "instrumented":true, "worker_id":null}),
            "wake" => {
                serde_json::json!({"event":"WakeEventEvent", "timestamp_ns":2, "waker_task_id":0,
                "woken_task_id":id, "target_worker":255})
            }
            "dump" => {
                serde_json::json!({"event":"TaskDumpEvent", "timestamp_ns":3, "task_id":id, "callchain":[]})
            }
            _ => panic!("invalid test event"),
        };
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn pairing_uses_recording_identity_and_not_file_order() {
        let mut value = Evidence::default();
        // A later segment is supplied first; another process reuses task ID 7.
        value.ingest(Path::new("one"), &[event("wake", 7), event("dump", 7)]);
        value.ingest(Path::new("two"), &[event("spawn", 7)]);
        assert_eq!(value.wake_pairs(), (0, 0));
        assert!(!value.failures(1, true, true).is_empty());
        value.ingest(Path::new("one"), &[event("spawn", 7)]);
        assert_eq!(value.instrumented.len(), 2);
        assert_eq!(value.wake_pairs(), (1, 1));
        assert!(value.failures(2, true, true).is_empty());
    }

    #[test]
    fn unmatched_dumps_cannot_pass_dump_assertion() {
        let mut value = Evidence::default();
        value.ingest(
            Path::new("one"),
            &[event("spawn", 1), event("dump", 1), event("dump", 2)],
        );
        assert!(!value.failures(1, false, true).is_empty());
        assert!(value.failures(1, false, false).is_empty());
    }

    #[test]
    fn aliases_and_directory_cycles_do_not_duplicate_segments() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("trace.1.bin");
        std::fs::write(&file, []).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), root.path().join("cycle")).unwrap();
        let mut files = BTreeSet::new();
        let mut visited = HashSet::new();
        collect_trace_files(root.path(), &mut files, &mut visited).unwrap();
        collect_trace_files(&file, &mut files, &mut visited).unwrap();
        assert_eq!(files.len(), 1);
    }
}

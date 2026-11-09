//! 采样循环：唯一会真正触发终止的地方。
//!
//! 性能约定：没有未封窗的会话、且内存未达阈值时，**不扫描 `/proc`**
//! （只在超阈值或启动窗口期间才读进程表）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::config::{Config, Mode};
use super::proc_tree::{ProcInfo, select_victim, subtree_targets};
use super::registry::{Registry, Root};
use super::switch::Switch;
use super::{cgroup, kill, registry, sys};

/// 启动单例采样任务。`registry` 需已通过 [`registry::install`] 安装。
pub fn start(config: Config) -> JoinHandle<()> {
    tokio::spawn(run(config))
}

pub async fn run(config: Config) {
    let Some(reg) = registry::global() else {
        tracing::warn!(
            event = "mem_watch.disabled",
            reason = "registry not installed; call mem_watch::registry::install() first"
        );
        return;
    };

    let mut switch = Switch::default();
    let mut last_action: Option<Instant> = None;
    let mut last_mode = Mode::Off;

    tracing::info!(
        event = "mem_watch.started",
        mode = config.mode.as_str(),
        ratio = config.ratio,
        interval_ms = config.interval.as_millis() as u64,
        min_ratio = config.min_ratio,
        grace_ms = config.grace.as_millis() as u64,
        cooldown_ms = config.cooldown.as_millis() as u64,
        bootstrap_cap_ms = config.bootstrap_cap.as_millis() as u64,
    );

    loop {
        let cfg = switch.effective(&config);
        if cfg.mode != last_mode {
            tracing::info!(
                event = "mem_watch.mode_changed",
                from = last_mode.as_str(),
                to = cfg.mode.as_str(),
            );
            last_mode = cfg.mode;
        }
        if cfg.mode != Mode::Off {
            step(&reg, &cfg, &mut last_action).await;
        }
        // 用 sleep 而不是 interval：开关文件可以改 interval_ms 并立刻生效。
        tokio::time::sleep(cfg.interval).await;
    }
}

async fn step(reg: &Arc<Registry>, cfg: &Config, last_action: &mut Option<Instant>) {
    let Some(limit) = sys::read_memory_max() else {
        return; // 无限制或非 Linux
    };

    let current = sys::read_memory_current().unwrap_or(0);
    let stat = sys::read_memory_stat().unwrap_or_default();
    let working_set = cgroup::working_set(current, &stat);
    let threshold = cfg.threshold(limit);

    let roots = reg.all();
    if roots.is_empty() {
        return;
    }
    let bootstrapping = roots.iter().any(|root| !root.is_sealed());

    // 快速路径：没有待封窗的会话、且未超阈值 → 不扫全表，
    // 但仍校验 root 存活，避免 pid 复用后把无关进程纳入淘汰范围。
    if !bootstrapping && working_set < threshold {
        let pruned = reg.prune_dead_light();
        if pruned > 0 {
            tracing::debug!(event = "mem_watch.pruned_dead_roots", count = pruned);
        }
        return;
    }

    let table = sys::read_all();
    if table.is_empty() {
        return;
    }

    if reg.prune_dead(&table) > 0 {
        tracing::debug!(event = "mem_watch.pruned_dead_roots");
    }

    if bootstrapping {
        reg.snapshot_bootstrap(&table);

        // 没有封窗钩子的 agent：到点强制封窗，否则永远不工作。
        for root in reg.all() {
            if !root.is_sealed() && root.spawned_at.elapsed() >= cfg.bootstrap_cap {
                if reg.seal_project(&root.project, &table) > 0 {
                    tracing::info!(
                        event = "mem_watch.bootstrap_timeout",
                        project = %root.project,
                        cap_ms = cfg.bootstrap_cap.as_millis() as u64,
                    );
                }
            }
        }
    }

    if working_set < threshold {
        return;
    }

    let roots = reg.all();
    let root_pids: Vec<u32> = roots.iter().map(|root| root.pid).collect();
    let protected = protected_set(&roots, &table);

    let Some(victim) = select_victim(&table, &root_pids, &protected, cfg.min_rss(limit)) else {
        tracing::debug!(
            event = "mem_watch.no_candidate",
            working_set_mb = working_set >> 20,
            limit_mb = limit >> 20,
        );
        return;
    };

    if cfg.mode == Mode::DryRun {
        // dry-run 也按冷却节流：否则超阈值期间每 tick 一条，一周的日志会淹没一切。
        if last_action.is_some_and(|at| at.elapsed() < cfg.cooldown) {
            return;
        }
        *last_action = Some(Instant::now());
        tracing::info!(
            event = "mem_watch.would_evict",
            pid = victim.pid,
            rss_mb = victim.rss_bytes >> 20,
            comm = %victim.comm,
            working_set_mb = working_set >> 20,
            limit_mb = limit >> 20,
        );
        return;
    }

    if last_action.is_some_and(|at| at.elapsed() < cfg.cooldown) {
        return;
    }
    *last_action = Some(Instant::now());

    tracing::warn!(
        event = "mem_watch.evict",
        pid = victim.pid,
        rss_mb = victim.rss_bytes >> 20,
        comm = %victim.comm,
        working_set_mb = working_set >> 20,
        limit_mb = limit >> 20,
    );

    // 连子树一起杀：只杀父进程会让子进程 reparent 到 PID 1，
    // 脱离 descendants(root) 范围后既不会被回收也不会再被选中。
    let targets = subtree_targets(&table, victim.pid, &protected);
    tracing::warn!(
        event = "mem_watch.evict_targets",
        victim = victim.pid,
        targets = targets.len(),
    );

    // 终止序列独立执行，不能阻塞采样：grace 期间可能还有第二个尖峰。
    let grace = cfg.grace;
    tokio::spawn(async move {
        kill::terminate_many(&targets, grace).await;
    });
}

/// 永不入选的进程：PID 1、所有会话的启动窗口快照、以及 exe 命中规则的进程。
fn protected_set(roots: &[Arc<Root>], table: &HashMap<u32, ProcInfo>) -> HashSet<u32> {
    let mut protected: HashSet<u32> = HashSet::from([1u32]);
    for root in roots {
        // 只保护 starttime 仍匹配的 pid：pid 被复用后的新进程不能继承保护
        for (pid, born) in root.protected() {
            if table.get(&pid).is_some_and(|info| info.starttime == born) {
                protected.insert(pid);
            }
        }
    }
    for pid in table.keys() {
        if protected.contains(pid) {
            continue;
        }
        if let Some(exe) = sys::read_exe(*pid) {
            if roots.iter().any(|root| root.matches_exe(&exe)) {
                protected.insert(*pid);
            }
        }
    }
    protected
}

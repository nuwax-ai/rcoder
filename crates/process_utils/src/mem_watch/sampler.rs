//! 采样循环：唯一会真正触发终止的地方。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::config::{Config, Mode};
use super::proc_tree::{ProcInfo, select_victim};
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
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_kill: Option<Instant> = None;

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
        ticker.tick().await;
        let cfg = switch.effective(&config);
        if cfg.mode == Mode::Off {
            continue;
        }
        step(&reg, &cfg, &mut last_kill).await;
    }
}

async fn step(reg: &Arc<Registry>, cfg: &Config, last_kill: &mut Option<Instant>) {
    let Some(limit) = sys::read_memory_max() else {
        return; // 无限制或非 Linux
    };
    let table = sys::read_all();
    if table.is_empty() {
        return;
    }

    reg.prune_dead(&table);
    reg.snapshot_bootstrap(&table);

    // 没有封窗钩子的 agent：到点强制封窗，否则永远不工作。
    for root in reg.all() {
        if !root.is_sealed() && root.spawned_at.elapsed() >= cfg.bootstrap_cap {
            if reg.seal(&root.project, &table) {
                tracing::info!(
                    event = "mem_watch.bootstrap_timeout",
                    project = %root.project,
                    cap_ms = cfg.bootstrap_cap.as_millis() as u64,
                );
            }
        }
    }

    let current = sys::read_memory_current().unwrap_or(0);
    let stat = sys::read_memory_stat().unwrap_or_default();
    let working_set = cgroup::working_set(current, &stat);
    let threshold = cfg.threshold(limit);
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

    if last_kill.is_some_and(|at| at.elapsed() < cfg.cooldown) {
        return;
    }
    *last_kill = Some(Instant::now());

    tracing::warn!(
        event = "mem_watch.evict",
        pid = victim.pid,
        rss_mb = victim.rss_bytes >> 20,
        comm = %victim.comm,
        working_set_mb = working_set >> 20,
        limit_mb = limit >> 20,
    );

    // 终止序列独立执行，不能阻塞采样：3s 的 grace 期间可能还有第二个尖峰。
    tokio::spawn(kill::terminate(victim.pid, cfg.grace));
}

/// 永不入选的进程：PID 1、所有会话的启动窗口快照、以及 exe 命中规则的进程。
fn protected_set(roots: &[Arc<Root>], table: &HashMap<u32, ProcInfo>) -> HashSet<u32> {
    let mut protected: HashSet<u32> = HashSet::from([1u32]);
    for root in roots {
        protected.extend(root.protected());
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

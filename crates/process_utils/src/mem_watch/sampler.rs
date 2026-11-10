//! 采样循环：唯一会真正触发终止的地方。
//!
//! 性能约定：没有未封窗的会话、且内存未达阈值时，**不扫描 `/proc`**
//! （只在超阈值或启动窗口期间才读进程表）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use super::config::{Config, Mode};
use super::proc_tree::{ProcInfo, select_victim, subtree_targets};
use super::registry::{Registry, Root};
use super::switch::Switch;
use super::{cgroup, kill, registry, sys};

/// LSM 拒绝信号后的全局降级标志（kill EPERM 时置位）。
static DISABLED: AtomicBool = AtomicBool::new(false);

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
    // cgroup 归属启动时解析一次（进程生命周期内不变）。
    let probe = cgroup::CgroupProbe::resolve();
    // 口径异常只报一次：否则每 tick 一条，日志会被同一条原因淹没。
    let mut scope_warned = false;

    tracing::info!(
        event = "mem_watch.started",
        mode = config.mode.as_str(),
        ratio = config.ratio,
        interval_ms = config.interval.as_millis() as u64,
        min_ratio = config.min_ratio,
        grace_ms = config.grace.as_millis() as u64,
        cooldown_ms = config.cooldown.as_millis() as u64,
        bootstrap_cap_ms = config.bootstrap_cap.as_millis() as u64,
        cgroup_relpath = %probe.relpath(),
        cgroup_max_bytes = probe.max(),
        cgroup_unresolved = probe.unresolved(),
    );

    loop {
        if DISABLED.load(Ordering::Relaxed) {
            // EPERM 降级后不再采样（保留 started/mode_changed 历史日志可查）
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
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
            step(&reg, &cfg, &probe, &mut last_action, &mut scope_warned).await;
        }
        // 用 sleep 而不是 interval：开关文件可以改 interval_ms 并立刻生效。
        tokio::time::sleep(cfg.interval).await;
    }
}

/// 口径校验：用量与限额必须来自同一 cgroup 组。
///
/// 返回 `false` 时调用方必须放弃本次淘汰。两类拒绝：
/// 1. 定位失败（读不到 `/proc/self/cgroup` v2 行）；
/// 2. env 覆盖的限额与实测 `memory.max` 不一致——典型是 host cgroup namespace
///    下根路径给出整机用量，却和容器限额比大小。
fn scope_ok(cfg: &Config, probe: &cgroup::CgroupProbe, limit: u64, warned: &mut bool) -> bool {
    if cfg.limit_override.is_none() {
        // 没有覆盖：限额就是从本组读出来的，天然同源
        return true;
    }
    if probe.confirms(limit) {
        return true;
    }
    if !*warned {
        *warned = true;
        tracing::error!(
            event = "mem_watch.scope_mismatch",
            reason = probe
                .unresolved()
                .unwrap_or("limit override does not match this cgroup's memory.max"),
            limit_bytes = limit,
            cgroup_max_bytes = probe.max(),
            cgroup_relpath = %probe.relpath(),
            hint = "host cgroup namespace 下根路径是整机用量;确认用量与限额同组后再启用淘汰",
        );
    }
    false
}

async fn step(
    reg: &Arc<Registry>,
    cfg: &Config,
    probe: &cgroup::CgroupProbe,
    last_action: &mut Option<Instant>,
    scope_warned: &mut bool,
) {
    let Some(limit) = cfg.effective_limit() else {
        // 无限额信息：host cgroup namespace（容器内 memory.max=max）且没有
        // env 覆盖——看门狗无法判断阈值，保持空闲（安全方向）。
        return;
    };
    if !scope_ok(cfg, probe, limit, scope_warned) {
        return;
    }

    // 用量读不到就放弃本轮：不能退化成 0（那会让保护静默失效），
    // 也不能拿上次的旧值继续判断。
    let Some(current) = sys::read_memory_current(probe) else {
        if !*scope_warned {
            *scope_warned = true;
            tracing::warn!(
                event = "mem_watch.usage_unreadable",
                path = %probe.current_path().display(),
                hint = "cgroup 用量不可读;淘汰暂停直至恢复",
            );
        }
        return;
    };
    let stat = sys::read_memory_stat(probe).unwrap_or_default();
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
            if !root.is_sealed()
                && root.spawned_at.elapsed() >= cfg.bootstrap_cap
                && reg.seal_project(&root.project, &table) > 0
            {
                tracing::info!(
                    event = "mem_watch.bootstrap_timeout",
                    project = %root.project,
                    cap_ms = cfg.bootstrap_cap.as_millis() as u64,
                );
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
    // 目标携带快照 starttime，终止前复核身份，避免 PID 复用后误杀。
    let targets = subtree_targets(&table, victim.pid, &protected);
    tracing::warn!(
        event = "mem_watch.evict_targets",
        victim = victim.pid,
        targets = targets.len(),
    );

    // 终止序列独立执行，不能阻塞采样：grace 期间可能还有第二个尖峰。
    // 但其结果必须监督：EPERM（LSM 拒绝 signal）意味着看门狗杀不掉进程，
    // 若不降级会永远刷 evict 日志——"以为在工作其实杀不掉"是最坏的失效形态。
    let grace = cfg.grace;
    let kill_task = tokio::spawn(async move { kill::terminate_many(&targets, grace).await });
    tokio::spawn(async move {
        if let Ok(joined) = kill_task.await {
            if let Some(err) = joined.denied() {
                tracing::error!(
                    event = "mem_watch.disabled",
                    reason = "signal denied by LSM (EPERM)",
                    hint = "apparmor/SELinux 拒绝容器内发信号；检查 dmesg 与 pod appArmorProfile",
                    pid = err.pid,
                    error = %err.detail,
                    report = %joined.summary(),
                );
                DISABLED.store(true, Ordering::Relaxed);
            } else if !joined.is_clean() {
                // 有目标没杀动但不是 LSM 拒绝：不降级（可能是瞬时错误），
                // 但要留痕，否则"部分失败"会被 term_ok 掩盖。
                tracing::error!(
                    event = "mem_watch.terminate_partial_failure",
                    report = %joined.summary(),
                );
            }
        }
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
        if let Some(exe) = sys::read_exe(*pid)
            && roots.iter().any(|root| root.matches_exe(&exe))
        {
            protected.insert(*pid);
        }
    }
    protected
}

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
fn scope_ok(
    probe: &cgroup::CgroupProbe,
    limit: u64,
    group_max: Option<u64>,
    warned: &mut bool,
) -> bool {
    // 不论限额来自 env 覆盖还是本组 memory.max，都要求与本组**当前**限额一致。
    //
    // `group_max` 必须是本轮新读的值：`probe.max` 是启动快照，运行中调整限额
    // （4Gi→6Gi）后用它比对会永远不匹配，看门狗从此每轮拒绝工作，直到重启。
    // 组路径（relpath）才是需要缓存的不变量。
    if group_max == Some(limit) {
        return true;
    }
    if !*warned {
        *warned = true;
        tracing::error!(
            event = "mem_watch.scope_mismatch",
            reason = probe.unresolved().unwrap_or(
                "limit does not match this cgroup's memory.max; usage and limit are not the same group"
            ),
            limit_bytes = limit,
            group_max_now = group_max,
            cached_startup_max = probe.max(),
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
    // 每轮重新读本组限额：限额可以在运行中调整，启动快照不能作为归属证明
    let group_max = sys::read_memory_max(probe);
    let Some(limit) = cfg.effective_limit(group_max) else {
        // 无限额信息：host cgroup namespace（容器内 memory.max=max）且没有
        // env 覆盖——看门狗无法判断阈值，保持空闲（安全方向）。
        return;
    };
    if !scope_ok(probe, limit, group_max, scope_warned) {
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

/// 当前看门狗真正能淘汰的 pid 集合。
///
/// 与 [`crate::mem_watch::sampler`] 选 victim 用的是同一条路：解析全局注册表
/// → 取 root → 算 protected → [`proc_tree::evictable_scope`]。资源接口
/// (`/computer/pod/resources` 的进程明细)用它给"会话工作负载"打标签，
/// 因此标签与看门狗行为严格一致。
///
/// 看门狗未安装、或没有任何已注册会话时返回空集——此时"没有可淘汰范围"
/// 是事实，不该把进程猜成负载。
pub fn evictable_pids(table: &HashMap<u32, ProcInfo>) -> HashSet<u32> {
    let Some(reg) = registry::global() else {
        return HashSet::new();
    };
    let roots = reg.all();
    if roots.is_empty() {
        return HashSet::new();
    }
    let root_pids: Vec<u32> = roots.iter().map(|root| root.pid).collect();
    let protected = protected_set(&roots, table);
    super::proc_tree::evictable_scope(table, &root_pids, &protected)
}

/// 永不入选的进程：PID 1、所有会话的启动窗口快照、以及 agent 运行时家族。
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
        let cmdline = sys::read_cmdline(*pid).join(" ");
        let exe = sys::read_exe(*pid);
        if is_protected_by_identity(roots, exe.as_deref(), &cmdline) {
            protected.insert(*pid);
        }
    }
    protected
}

/// 单个进程是否因"身份"而受保护（除 PID 1 与会话启动窗口快照之外的两条规则）。
///
/// - 命令行命中 agent 运行时家族（claude/MCP/file-server…）→ 保护；
/// - `exe` 命中会话的安装路径（`script_hint`，或专用二进制相等）→ 保护。
///
/// **不能用解释器 exe 相等判身份**：node/python 的 `/proc/<pid>/exe` 是解释器
/// 本体，容器里所有同语言进程同路径，相等判定会让 agent 自己 spawn 的
/// node/python 负载永远杀不掉（详见 `registry::Root::matches_exe`）。
fn is_protected_by_identity(
    roots: &[Arc<Root>],
    exe: Option<&std::path::Path>,
    cmdline: &str,
) -> bool {
    if registry::is_agent_runtime(cmdline) {
        return true;
    }
    exe.is_some_and(|exe| roots.iter().any(|root| root.matches_exe(exe)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_watch::cgroup::CgroupProbe;

    fn probe(max: Option<u64>, unresolved: Option<&'static str>) -> CgroupProbe {
        CgroupProbe::for_test("", max, unresolved)
    }

    fn node_root() -> Vec<Arc<Root>> {
        vec![Arc::new(Root::new(
            "p",
            100,
            42,
            std::path::PathBuf::from("/usr/local/bin/node"),
            "claude-code-acp-ts".into(),
        ))]
    }

    #[test]
    fn interpreter_family_is_not_blindly_protected() {
        let roots = node_root();
        // 无关 node 负载：exe 与 agent 根相同，但命令行不属于 agent 家族
        // → 必须可淘汰（修复前会被 exe 相等规则误保护）
        assert!(!is_protected_by_identity(
            &roots,
            Some(std::path::Path::new("/usr/local/bin/node")),
            "node /app/webpack-dev-server.js",
        ));
        assert!(!is_protected_by_identity(
            &roots,
            Some(std::path::Path::new("/usr/local/bin/node")),
            "node /app/node_modules/.bin/tsc --watch",
        ));
    }

    #[test]
    fn agent_runtime_and_install_path_stay_protected() {
        let roots = node_root();
        // agent 家族子进程（同解释器，但命令行命中标记）→ 保护
        assert!(is_protected_by_identity(
            &roots,
            Some(std::path::Path::new("/usr/local/bin/node")),
            "node /usr/local/lib/node_modules/claude-code-acp-ts/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs",
        ));
        // 安装路径下的子二进制 → exe 命中 script_hint → 保护
        assert!(is_protected_by_identity(
            &roots,
            Some(std::path::Path::new(
                "/usr/local/lib/node_modules/claude-code-acp-ts/node_modules/x/claude"
            )),
            "claude --resume",
        ));
    }

    #[test]
    fn python_root_does_not_shield_python_children() {
        // python agent 同理：它 spawn 的普通 python 工具必须可淘汰
        let roots = vec![Arc::new(Root::new(
            "p",
            200,
            7,
            std::path::PathBuf::from("/usr/bin/python3.13"),
            "main.py".into(),
        ))];
        assert!(!is_protected_by_identity(
            &roots,
            Some(std::path::Path::new("/usr/bin/python3.13")),
            "python3 heavy_tool.py",
        ));
        assert!(!is_protected_by_identity(
            &roots,
            Some(std::path::Path::new("/usr/bin/python3.13")),
            "python3 -m websockify --web /opt/noVNC 6080 localhost:5900",
        ));
    }

    #[test]
    fn scope_ok_rejects_mismatched_limit_override() {
        // host cgroupns 误杀的防线:覆盖限额与实测 memory.max 不一致 → 拒绝淘汰
        let p = probe(Some(4 << 30), None);
        let mut warned = false;
        assert!(
            !scope_ok(&p, 512 << 20, Some(4 << 30), &mut warned),
            "限额来自别处时必须拒绝"
        );
        assert!(warned, "拒绝必须留痕(且只报一次)");

        // 第二次沿用同一标记,不重复告警
        let before = warned;
        assert!(!scope_ok(&p, 512 << 20, Some(4 << 30), &mut warned));
        assert_eq!(warned, before);
    }

    /// 回归（复审 P2）：运行中把限额从 4Gi 调到 6Gi 后，看门狗必须继续工作。
    ///
    /// 原实现用启动快照 `probe.max` 证明"当前归属"，限额一变就永远不匹配，
    /// 之后每轮都拒绝，直到进程重启。现在用本轮新读的 `group_max`，
    /// 只要限额来自本组就放行。
    #[test]
    fn scope_ok_follows_runtime_limit_change() {
        // probe 里缓存的是启动时的 4Gi
        let p = probe(Some(4 << 30), None);

        // 运行中已调到 6Gi：本轮读到的 group_max 是 6Gi，limit 也来自本组
        let mut warned = false;
        assert!(
            scope_ok(&p, 6 << 30, Some(6 << 30), &mut warned),
            "限额调整后应继续放行(用当前值而非启动快照)"
        );
        assert!(!warned);

        // 若 env 覆盖仍是旧的 4Gi，而本组现在是 6Gi → 必须拒绝(不同源)
        let mut warned = false;
        assert!(
            !scope_ok(&p, 4 << 30, Some(6 << 30), &mut warned),
            "覆盖值与当前本组限额不一致时必须拒绝"
        );
        assert!(warned);
    }

    #[test]
    fn scope_ok_rejects_unresolved_probe() {
        let broken = probe(None, Some("cannot read /proc/self/cgroup"));
        let mut warned = false;
        assert!(
            !scope_ok(&broken, 4 << 30, None, &mut warned),
            "定位失败一律拒绝"
        );
        assert!(warned);
    }

    #[test]
    fn scope_ok_accepts_limit_matching_this_group() {
        // 限额与本组实测一致 → 放行（不论来自本组 memory.max 还是 env 覆盖）
        let p = probe(Some(4 << 30), None);
        let mut warned = false;
        assert!(scope_ok(&p, 4 << 30, Some(4 << 30), &mut warned));
        assert!(!warned, "同源时不该告警");
    }

    /// 回归（审查 P2-3）：无 env override 时，若传入限额与本组实测不一致，
    /// 必须拒绝而不是放行。
    ///
    /// 原实现的前提是"没有 override ⇒ 限额就是从本组读出来的"，但限额当时读的是
    /// 固定根路径 `/sys/fs/cgroup/memory.max`。host cgroup namespace 下根是宿主
    /// 整机限额，与容器子组不同源 —— 必须拒绝，否则就是"容器用量比宿主限额"。
    #[test]
    fn scope_ok_rejects_mismatch_without_override() {
        let p = probe(Some(4 << 30), None);
        let mut warned = false;
        assert!(
            !scope_ok(&p, 1 << 30, Some(4 << 30), &mut warned),
            "限额与本组 memory.max 不同源时必须拒绝"
        );
        assert!(warned, "拒绝时应记录一次 scope_mismatch");
    }

    /// 根路径 memory.max 为 `max`（probe 拿不到有限限额）时也不得放行。
    #[test]
    fn scope_ok_rejects_when_limit_unknown() {
        let p = probe(None, None);
        let mut warned = false;
        assert!(!scope_ok(&p, 4 << 30, None, &mut warned));
    }
}

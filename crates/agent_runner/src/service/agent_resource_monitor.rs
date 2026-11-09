//! agent 进程组资源采集(procfs 按 pgid 匹配进程组)+ 整容器资源采集
//! (cgroup v2)。
//!
//! 查询驱动采样:每次 `GetContainerStatus` 调用一次,与上次采样基线做
//! CPU 时间差分;无常驻采样任务,无人查询零开销。
//!
//! 降级语义(与 `GetContainerStatusResponse.cpu_percent/memory_mb` 的既定
//! 契约一致):agent 未运行、实现方不暴露 pgid、平台不支持(非 Linux)、
//! /proc 读取失败、采样间隔不足(<1s)、执行身份不符时字段返回 None,
//! RPC 不因此报错。
//!
//! 换代安全:pgid 与执行身份(session_id)每次从 [`AGENT_REGISTRY`] 注册表
//! 同一读取中成对获取,不缓存;CPU 基线绑定 (身份, pgid) 双元组——会话换代、
//! 同会话换进程(session/load 恢复的新进程沿用同一 ID)、ticks 回退都丢弃
//! 差分并重置水位,绝不跨代计算、绝不返回负值。/proc 扫描结束后复核身份
//! 仍未变化,扫描期间发生换代的迟到样本直接丢弃,不覆盖新基线。

#[cfg(any(target_os = "linux", test))]
use std::sync::LazyLock;
#[cfg(any(target_os = "linux", test))]
use std::time::Instant;

#[cfg(any(target_os = "linux", test))]
use dashmap::DashMap;

use crate::service::AGENT_REGISTRY;

/// agent 进程组 CPU 差分基线(project_id → 上次采样)。
///
/// DashMap 有锁:一律走 entry API,临界区内只做内存读写,无 I/O、无 await。
/// 基线机制仅 Linux 采样路径与测试使用;非 Linux 非 test 构建下
/// [`invalidate_baseline`] 为空操作。
#[cfg(any(target_os = "linux", test))]
static CPU_BASELINES: LazyLock<DashMap<String, CpuBaseline>> = LazyLock::new(DashMap::new);

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct CpuBaseline {
    /// 基线所属的执行身份(registry session_id;字段名避开保留字 gen)。
    /// 注意 session/load 恢复的新进程会沿用同一 ID,故还需 pgid 区分进程代次。
    execution: String,
    /// 基线所属的进程组(组长 PID)。同会话换进程时 pgid 变化,旧基线不可比。
    pgid: u32,
    /// 进程组 utime+stime 累计值(clock ticks)
    ticks: u64,
    at: Instant,
}

/// 清除 project 的过期 CPU 基线。
#[cfg(any(target_os = "linux", test))]
fn invalidate_baseline(project_id: &str) {
    CPU_BASELINES.remove(project_id);
}

/// 非 Linux 非 test 构建:无基线机制,空操作。
#[cfg(not(any(target_os = "linux", test)))]
fn invalidate_baseline(_project_id: &str) {}

/// 单次 agent 进程组采样结果。
pub struct AgentResourceSample {
    /// 进程组内所有进程常驻内存(RSS)之和,MB(四舍五入)
    pub memory_mb: i64,
    /// 相对上次采样的 CPU 使用率(百分比);None = 首次采样/间隔不足/换代回退
    pub cpu_percent: Option<f64>,
    /// 采样瞬间进程组内可见进程数
    pub process_count: u32,
}

/// 按注册表当前执行采集指定 project 的 agent 进程组资源。
///
/// `expected_session`:调用方已确认的执行身份(如状态查询先经 GetStatus
/// 确认的会话)。registry 当前会话与其不符时不采集——状态与资源必须来自
/// 同一执行,换代时资源字段缺省。`None` 表示按当前代次采样。
///
/// 返回 `(cpu_percent, memory_mb)`;注册表无条目、身份不符、实现方无 pgid
/// 或采集失败时均为 `(None, None)`。DashMap Ref 在 /proc 扫描前显式释放,
/// 避免扫描期间阻塞注册表写方。
pub fn sample_for_project(
    project_id: &str,
    expected_session: Option<&str>,
) -> (Option<f64>, Option<i64>) {
    // 同一读取成对取出身份与 pgid,guard 内仅做拷贝,扫描前释放。
    let execution = AGENT_REGISTRY.get_agent_info(project_id).and_then(|info| {
        let execution = info.session_id.0.as_ref().to_owned();
        let pgid = info
            .stop_handle
            .as_ref()
            .and_then(|handle| handle.agent_pgid());
        drop(info);
        pgid.map(|pgid| (execution, pgid))
    });

    let Some((execution, pgid)) = execution else {
        // agent 未运行(或实现方不暴露 pgid):清掉过期基线,字段缺省。
        invalidate_baseline(project_id);
        return (None, None);
    };
    if let Some(expected) = expected_session
        && expected != execution
    {
        // 身份不符:调用方确认的执行已被换代替换,资源字段缺省。
        return (None, None);
    }

    match sample_agent_resources(project_id, &execution, pgid) {
        Some(sample) => (sample.cpu_percent, Some(sample.memory_mb)),
        None => {
            // 进程组已消失(退出竞态):基线一并失效。
            invalidate_baseline(project_id);
            (None, None)
        }
    }
}

/// 采集 pgid 进程组的资源快照。`(execution, pgid)` 绑定 CPU 差分基线。
/// 进程组无存活进程时返回 None。
#[cfg(target_os = "linux")]
pub fn sample_agent_resources(
    project_id: &str,
    execution: &str,
    pgid: u32,
) -> Option<AgentResourceSample> {
    prune_stale_baselines();

    let tps = procfs::ticks_per_second();
    let mut rss_bytes: u64 = 0;
    let mut total_ticks: u64 = 0;
    let mut process_count: u32 = 0;

    // 进程表统一走 process_utils 的公共快照：与内存看门狗共用同一份
    // /proc/<pid>/stat 解析（字段下标、comm 括号、rss 页→字节换算只维护一处）。
    // /proc 整体不可读时返回空表 ⇒ process_count == 0 ⇒ 按降级语义返回 None。
    let table = process_utils::proc_scan::snapshot();
    for info in table.values() {
        if info.pgrp != pgid {
            continue;
        }
        rss_bytes = rss_bytes.saturating_add(info.rss_bytes);
        total_ticks =
            total_ticks.saturating_add(info.utime_ticks.saturating_add(info.stime_ticks));
        process_count += 1;
    }

    if process_count == 0 {
        return None;
    }

    // 扫描结束复核:扫描期间发生换代(execution/pgid 变化)的迟到样本直接
    // 丢弃——样本可能混有旧组进程,不得计入也不得覆盖新代次基线。
    let still_current = AGENT_REGISTRY
        .get_agent_info(project_id)
        .map(|info| {
            let current = info.session_id.0.as_ref() == execution
                && info
                    .stop_handle
                    .as_ref()
                    .and_then(|handle| handle.agent_pgid())
                    == Some(pgid);
            drop(info);
            current
        })
        .unwrap_or(false);
    if !still_current {
        return None;
    }

    let now = Instant::now();
    let cpu_percent = advance_cpu_baseline(project_id, execution, pgid, total_ticks, now, tps).0;

    Some(AgentResourceSample {
        memory_mb: ((rss_bytes as f64 / 1_048_576.0).round()) as i64,
        cpu_percent,
        process_count,
    })
}

/// 非 Linux 平台:agent 容器均为 Linux;本地开发编译返回 None(与
/// vnc_activity 的门控模式一致),不破坏跨平台编译。
#[cfg(not(target_os = "linux"))]
pub fn sample_agent_resources(
    _project_id: &str,
    _execution: &str,
    _pgid: u32,
) -> Option<AgentResourceSample> {
    None
}

/// 推进 project 的 CPU 差分基线,返回 `(cpu_percent, 新基线)`。
///
/// 规则(纯函数,便于单测):
/// - 无基线(首次):记录水位,CPU% = None;
/// - 执行身份或进程组变化(会话换代/同会话换进程):旧基线不可比,
///   重置水位,CPU% = None;
/// - 距上次采样 < 1s:窗口太短,保留原基线,CPU% = None;
/// - ticks 回退(计数回绕):重置水位,CPU% = None;
/// - 正常:差分 / (tps × elapsed) × 100。多核/多进程下可超 100,
///   不封顶(封顶会掩盖真实占用,前端展示自行决定量程)。
///
/// 仅 Linux 采样路径与测试使用,避免非 Linux 非 test 构建的 dead code。
#[cfg(any(target_os = "linux", test))]
fn advance_cpu_baseline(
    project_id: &str,
    execution: &str,
    pgid: u32,
    total_ticks: u64,
    now: Instant,
    ticks_per_second: u64,
) -> (Option<f64>, Option<CpuBaseline>) {
    match CPU_BASELINES.entry(project_id.to_string()) {
        dashmap::mapref::entry::Entry::Vacant(vacant) => {
            let baseline = CpuBaseline {
                execution: execution.to_owned(),
                pgid,
                ticks: total_ticks,
                at: now,
            };
            vacant.insert(baseline.clone());
            (None, Some(baseline))
        }
        dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
            let baseline = occupied.get();
            if baseline.execution != execution || baseline.pgid != pgid {
                // 换代(会话变化或同会话换进程):新执行 ticks 从 0 重新累计,
                // 与旧水位不可比(多核下新组可很快越过旧水位,跨代差分是
                // 虚假数据)。
                let reset = CpuBaseline {
                    execution: execution.to_owned(),
                    pgid,
                    ticks: total_ticks,
                    at: now,
                };
                occupied.insert(reset.clone());
                return (None, Some(reset));
            }
            let baseline = baseline.clone();
            let elapsed = now.duration_since(baseline.at).as_secs_f64();
            if elapsed < 1.0 {
                // 保留原基线:下次查询将得到更长窗口的准确均值。
                return (None, Some(baseline));
            }
            if total_ticks < baseline.ticks {
                let reset = CpuBaseline {
                    execution: execution.to_owned(),
                    pgid,
                    ticks: total_ticks,
                    at: now,
                };
                occupied.insert(reset.clone());
                return (None, Some(reset));
            }
            let dticks = total_ticks - baseline.ticks;
            let percent = dticks as f64 / ticks_per_second as f64 / elapsed * 100.0;
            let updated = CpuBaseline {
                execution: execution.to_owned(),
                pgid,
                ticks: total_ticks,
                at: now,
            };
            occupied.insert(updated.clone());
            (Some(percent), Some(updated))
        }
    }
}

/// 修剪注册表已不存在的 project 基线(自愈,无需挂钩每个注册表移除点)。
#[cfg(target_os = "linux")]
fn prune_stale_baselines() {
    CPU_BASELINES.retain(|project_id, _| AGENT_REGISTRY.contains_project(project_id));
}

// ==== 整容器资源采集(cgroup v2) ====

/// 容器 CPU 差分基线。单容器单键,Mutex 足够;临界区仅内存读写,无 I/O、
/// 无 await。仅 Linux 采样路径使用(纯函数测试用 Option 直传,不碰此表)。
#[cfg(target_os = "linux")]
static CONTAINER_CPU_BASELINE: LazyLock<std::sync::Mutex<Option<ContainerCpuBaseline>>> =
    LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy)]
struct ContainerCpuBaseline {
    /// 容器 cgroup cpu.stat 的 usage_usec 累计值
    usage_usec: u64,
    at: Instant,
}

/// 单次整容器采样结果(cgroup v2)。
pub struct ContainerResourceSample {
    /// 容器 CPU 使用率(相对上次采样差分;多核可超 100);None = 首次/间隔不足
    pub cpu_percent: Option<f64>,
    /// 容器内存占用(memory.current),MB
    pub memory_mb: i64,
    /// 容器内存限额(memory.max),MB;"max"/不可得为 None
    pub memory_limit_mb: Option<i64>,
    /// 容器 CPU 核数限额(cpu.max quota/period),"max"/不可得为 None
    pub cpu_limit_cores: Option<f64>,
}

/// 采集整个容器(pod 计算容器)的资源用量。与换代无关——容器级统计不绑定
/// 执行身份。任何读取失败按降级语义返回 `(None, None, None, None)`。
pub fn sample_container_resources() -> (Option<f64>, Option<i64>, Option<i64>, Option<f64>) {
    #[cfg(target_os = "linux")]
    {
        match sample_container_resources_impl() {
            Some(sample) => (
                sample.cpu_percent,
                Some(sample.memory_mb),
                sample.memory_limit_mb,
                sample.cpu_limit_cores,
            ),
            None => {
                reset_container_baseline();
                (None, None, None, None)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 空操作(该构建下 reset 为空实现),保持分支对称。
        reset_container_baseline();
        (None, None, None, None)
    }
}

/// Linux 实现:读 cgroup v2(经 /proc/self/cgroup 定位容器自身组路径,
/// 兼容私有/宿主 cgroup 命名空间)。
#[cfg(target_os = "linux")]
fn sample_container_resources_impl() -> Option<ContainerResourceSample> {
    let root = container_cgroup_root()?;
    let memory_current = read_u64(&root.join("memory.current"))?;
    let memory_limit_bytes = match std::fs::read_to_string(root.join("memory.max")) {
        Ok(text) => match text.trim() {
            "max" | "" => None,
            digits => digits
                .parse::<u64>()
                .ok()
                .map(|bytes| (bytes as f64 / 1_048_576.0).round() as i64),
        },
        Err(_) => None,
    };
    let usage_usec = parse_usage_usec(&std::fs::read_to_string(root.join("cpu.stat")).ok()?)?;
    let cpu_limit_cores = match std::fs::read_to_string(root.join("cpu.max")) {
        Ok(text) => parse_cpu_max(&text),
        Err(_) => None,
    };

    let now = Instant::now();
    let cpu_percent = {
        // Mutex 临界区:仅内存读写,无 I/O、无 await。
        let mut guard = CONTAINER_CPU_BASELINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (percent, next) = advance_container_cpu(*guard, usage_usec, now);
        *guard = next;
        percent
    };

    Some(ContainerResourceSample {
        cpu_percent,
        memory_mb: (memory_current as f64 / 1_048_576.0).round() as i64,
        memory_limit_mb: memory_limit_bytes,
        cpu_limit_cores,
    })
}

/// 重置容器 CPU 基线(读取失败时调用,下次从零重建)。
#[cfg(target_os = "linux")]
fn reset_container_baseline() {
    if let Ok(mut guard) = CONTAINER_CPU_BASELINE.lock() {
        *guard = None;
    }
}

/// 非 Linux 构建(含其 test):无基线机制,空操作。
#[cfg(not(target_os = "linux"))]
fn reset_container_baseline() {}

/// 读非负整数文件(trim 后解析;失败返回 None)。
#[cfg(target_os = "linux")]
fn read_u64(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
}

/// 解析 cgroup v2 容器自身组路径:`/proc/self/cgroup` 的 `0::<path>` 行
/// (v2 统一层级)。私有命名空间下为 `/`,宿主命名空间下为容器真实相对路径。
#[cfg(target_os = "linux")]
fn container_cgroup_root() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let line = text.lines().find(|line| line.starts_with("0::"))?;
    let path = line.trim_start_matches("0::");
    let trimmed = path.trim();
    let relative = trimmed.trim_start_matches('/');
    if relative.is_empty() {
        Some(std::path::PathBuf::from("/sys/fs/cgroup"))
    } else {
        Some(std::path::PathBuf::from("/sys/fs/cgroup").join(relative))
    }
}

/// 从 cpu.stat 文本解析 `usage_usec`。
#[cfg(any(target_os = "linux", test))]
fn parse_usage_usec(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// 解析 cpu.max:`"<quota> <period>"` → 核数;"max"/不可解析 → None。
#[cfg(any(target_os = "linux", test))]
fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut parts = text.split_whitespace();
    let quota = parts.next()?;
    if quota == "max" {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    let period: u64 = parts.next().unwrap_or("100000").parse().ok()?;
    if period == 0 {
        return None;
    }
    Some(quota as f64 / period as f64)
}

/// 推进容器 CPU 差分基线(纯函数):首次/回退→None+重置;<1s→None 保留;
/// 正常差分,多核可超 100。
#[cfg(any(target_os = "linux", test))]
fn advance_container_cpu(
    baseline: Option<ContainerCpuBaseline>,
    usage_usec: u64,
    now: Instant,
) -> (Option<f64>, Option<ContainerCpuBaseline>) {
    let Some(baseline) = baseline else {
        return (
            None,
            Some(ContainerCpuBaseline {
                usage_usec,
                at: now,
            }),
        );
    };
    let elapsed = now.duration_since(baseline.at).as_secs_f64();
    if elapsed < 1.0 {
        return (None, Some(baseline));
    }
    if usage_usec < baseline.usage_usec {
        return (
            None,
            Some(ContainerCpuBaseline {
                usage_usec,
                at: now,
            }),
        );
    }
    let d_usec = (usage_usec - baseline.usage_usec) as f64;
    let percent = d_usec / 1_000_000.0 / elapsed * 100.0;
    (
        Some(percent),
        Some(ContainerCpuBaseline {
            usage_usec,
            at: now,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小生命周期替身:暴露指定 pgid,其余行为为空实现。
    /// 仅 Linux 门控测试使用(own_process_group)。
    #[cfg(target_os = "linux")]
    mod lifecycle_test_support {
        use std::future::Future;
        use std::pin::Pin;

        use super::*;

        pub(super) struct TestLifecycle {
            pub pgid: u32,
            pub token: tokio_util::sync::CancellationToken,
        }

        impl shared_types::AgentLifecycle for TestLifecycle {
            fn graceful_stop(
                &self,
            ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
                Box::pin(async { Ok(()) })
            }
            fn wait_for_exit(
                &self,
            ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
                Box::pin(async { Ok(()) })
            }
            fn cancel(&self) {}
            fn is_stopped(&self) -> bool {
                false
            }
            fn agent_pgid(&self) -> Option<u32> {
                Some(self.pgid)
            }
            fn cancellation_token(&self) -> &tokio_util::sync::CancellationToken {
                &self.token
            }
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    use lifecycle_test_support::TestLifecycle;

    fn tps() -> u64 {
        100 // clock ticks/s 的常见值(_SC_CLK_TCK=100);纯函数测试用固定值
    }

    #[test]
    fn first_baseline_records_no_percent() {
        let now = Instant::now();
        let (percent, baseline) = advance_cpu_baseline("p-first", "exec-A", 101, 1_000, now, tps());
        assert_eq!(percent, None);
        assert_eq!(baseline.map(|b| b.ticks), Some(1_000));
    }

    #[test]
    fn short_interval_keeps_old_baseline() {
        let now = Instant::now();
        let (_, baseline) = advance_cpu_baseline("p-short", "exec-A", 101, 1_000, now, tps());
        let baseline = baseline.expect("baseline recorded");
        // 立即再次采样(模拟 <1s 间隔):CPU% None,基线不推进。
        let (percent, updated) =
            advance_cpu_baseline("p-short", "exec-A", 101, 1_500, Instant::now(), tps());
        assert_eq!(percent, None);
        assert_eq!(updated.map(|b| b.ticks), Some(baseline.ticks));
    }

    #[test]
    fn normal_diff_computes_percent() {
        let now = Instant::now();
        advance_cpu_baseline(
            "p-diff",
            "exec-A",
            101,
            1_000,
            now - std::time::Duration::from_secs(4),
            tps(),
        );
        // 400 ticks / 100tps = 4 CPU 秒,窗口 4s → 100%
        let (percent, updated) =
            advance_cpu_baseline("p-diff", "exec-A", 101, 1_400, Instant::now(), tps());
        let percent = percent.expect("percent after >1s window");
        assert!((percent - 100.0).abs() < 1.0, "percent={percent}");
        assert_eq!(updated.map(|b| b.ticks), Some(1_400));
    }

    #[test]
    fn tick_regression_resets_without_negative_percent() {
        let now = Instant::now();
        advance_cpu_baseline(
            "p-reset",
            "exec-A",
            101,
            5_000,
            now - std::time::Duration::from_secs(4),
            tps(),
        );
        // 同代计数回绕:低于旧水位。
        let (percent, updated) =
            advance_cpu_baseline("p-reset", "exec-A", 101, 200, Instant::now(), tps());
        assert_eq!(percent, None, "回退不得产生差分值");
        assert_eq!(updated.map(|b| b.ticks), Some(200), "水位应重置");
    }

    #[test]
    fn generation_change_rejects_stale_baseline_even_when_ticks_advance() {
        // Codex R1 反例:旧执行基线 1000 ticks;换代后新组(多核 4 秒)累计
        // 1400 ticks —— 数值上"上涨",但两个样本来自不同执行,差分无效。
        let now = Instant::now();
        advance_cpu_baseline(
            "p-cross",
            "exec-A",
            101,
            1_000,
            now - std::time::Duration::from_secs(4),
            tps(),
        );
        let (percent, updated) =
            advance_cpu_baseline("p-cross", "exec-B", 202, 1_400, Instant::now(), tps());
        assert_eq!(percent, None, "跨代差分不得产生 CPU%");
        let updated = updated.expect("watermark reset to new generation");
        assert_eq!(updated.execution, "exec-B", "基线应重置为新执行身份");
        assert_eq!(updated.pgid, 202);
        assert_eq!(updated.ticks, 1_400);
        // 新身份的下一次正常差分照常工作(基线已是 exec-B)。
        let (percent, _) =
            advance_cpu_baseline("p-cross", "exec-B", 202, 1_800, Instant::now(), tps());
        assert_eq!(percent, None, "窗口 <1s,仅推进基线");
    }

    #[test]
    fn same_session_new_process_rejects_stale_baseline() {
        // Codex R2 反例:session/load 恢复的新进程沿用同一会话 ID,但 pgid
        // 变化(进程换代)——ticks 数值上涨也不得跨代差分。
        let now = Instant::now();
        advance_cpu_baseline(
            "p-same-sid",
            "exec-same",
            101,
            1_000,
            now - std::time::Duration::from_secs(4),
            tps(),
        );
        let (percent, updated) =
            advance_cpu_baseline("p-same-sid", "exec-same", 202, 1_400, Instant::now(), tps());
        assert_eq!(percent, None, "同会话换进程不得跨代差分");
        let updated = updated.expect("watermark reset");
        assert_eq!(updated.execution, "exec-same");
        assert_eq!(updated.pgid, 202, "水位应绑定新进程组");
    }

    #[test]
    fn container_cpu_baseline_and_diff() {
        let t0 = Instant::now();
        // 首次:None。
        let (percent, next) = advance_container_cpu(None, 1_000_000, t0);
        assert_eq!(percent, None);
        // t0+4s:消耗 2 CPU 秒(2_000_000 usec)→ 50%。
        let (percent, next) =
            advance_container_cpu(next, 3_000_000, t0 + std::time::Duration::from_secs(4));
        let percent = percent.expect("container percent");
        assert!((percent - 50.0).abs() < 1.0, "percent={percent}");
        // t0+5s 回退(容器重启,usage 归零重计):None + 重置。
        let (percent, next) =
            advance_container_cpu(next, 100_000, t0 + std::time::Duration::from_secs(5));
        assert_eq!(percent, None);
        assert_eq!(next.expect("reset").usage_usec, 100_000);
    }

    #[test]
    fn parse_cpu_max_variants() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("150000 100000\n"), Some(1.5));
    }

    #[test]
    fn parse_usage_usec_variants() {
        assert_eq!(
            parse_usage_usec("usage_usec 467948671\nuser_usec 1\n"),
            Some(467_948_671)
        );
        assert_eq!(parse_usage_usec("nr_thawed 0\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_pgid_samples_none() {
        // 不存在的 pgid:None 且不产生基线副作用(幂等)。
        assert!(sample_agent_resources("p-unknown-pgid", "exec-x", u32::MAX - 1).is_none());
        assert!(!CPU_BASELINES.contains_key("p-unknown-pgid"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_group_samples_positive_memory() {
        // 测试进程自身所在进程组:RSS 求和必然 > 0,进程数 ≥ 1。
        // 扫描后复核依赖 AGENT_REGISTRY 当前 (session, pgid) 一致,须注册
        // 与采样声明相同的身份。
        let pgrp = procfs::process::Process::myself()
            .expect("self process")
            .stat()
            .expect("self stat")
            .pgrp as u32;
        AGENT_REGISTRY.register(
            "p-own-group",
            "exec-self",
            shared_types::ProjectAndAgentInfo {
                project_id: "p-own-group".to_string(),
                session_id: agent_client_protocol::schema::v1::SessionId::new(
                    std::sync::Arc::from("exec-self"),
                ),
                prompt_tx: tokio::sync::mpsc::channel(1).0,
                cancel_tx: tokio::sync::mpsc::channel(1).0,
                model_provider: None,
                request_id: None,
                status: crate::model::AgentStatus::Idle,
                last_activity: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
                stop_handle: Some(std::sync::Arc::new(TestLifecycle {
                    pgid: pgrp,
                    token: tokio_util::sync::CancellationToken::new(),
                })),
                agent_binary_snapshot: None,
            },
        );
        let sample =
            sample_agent_resources("p-own-group", "exec-self", pgrp).expect("own group sample");
        assert!(sample.memory_mb > 0, "memory_mb={}", sample.memory_mb);
        assert!(sample.process_count >= 1);
        // 首次采样 CPU% 为 None(基线刚建立)。
        assert_eq!(sample.cpu_percent, None);
        assert!(CPU_BASELINES.contains_key("p-own-group"));
        // 复核身份通过;清理注册残留。
        AGENT_REGISTRY.remove_by_project("p-own-group");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn container_sample_reads_cgroup_or_degrades() {
        // 真实容器内(cgroup v2):内存 > 0 且限额正确(compose agent_config
        // 设了 4GB/2 核);非容器环境读取失败 → 全 None,不 panic。
        let (cpu, mem, mem_limit, cpu_limit) = sample_container_resources();
        if let Some(mem) = mem {
            assert!(mem > 0, "container memory must be positive");
            if let Some(limit) = mem_limit {
                assert!(limit >= mem, "limit {limit}MB must cover usage {mem}MB");
            }
            assert!(cpu.is_none(), "first container sample has no baseline");
            if let Some(cores) = cpu_limit {
                assert!(cores > 0.0);
            }
        }
    }
}

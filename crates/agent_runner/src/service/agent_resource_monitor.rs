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
///
/// - `owner = None`：注册表里已没有任何执行，整键删除（不存在误删新代次的风险）；
/// - `owner = Some((execution, pgid))`：只删**属于该执行**的基线。迟到的旧采样
///   （扫描期间已换代）不能把新执行刚建立的基线一起删掉——那会让新代次的
///   第一次 CPU% 白白丢失。
#[cfg(any(target_os = "linux", test))]
fn invalidate_baseline(project_id: &str, owner: Option<(&str, u32)>) {
    match owner {
        None => {
            CPU_BASELINES.remove(project_id);
        }
        Some((execution, pgid)) => {
            CPU_BASELINES.remove_if(project_id, |_, baseline| {
                baseline.execution == execution && baseline.pgid == pgid
            });
        }
    }
}

/// 非 Linux 非 test 构建:无基线机制,空操作。
#[cfg(not(any(target_os = "linux", test)))]
fn invalidate_baseline(_project_id: &str, _owner: Option<(&str, u32)>) {}

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
        invalidate_baseline(project_id, None);
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
            // 进程组已消失(退出竞态)或扫描期间换代:基线一并失效。
            // 只删属于本次采样的那条:换代后新执行可能已建好基线,
            // 迟到的旧样本不能把它一起清掉。
            invalidate_baseline(project_id, Some((&execution, pgid)));
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
        total_ticks = total_ticks.saturating_add(info.utime_ticks.saturating_add(info.stime_ticks));
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
/// - **基线比本次采样还新**(`baseline.at > now`):说明已有更晚的采样(换代后的
///   新执行)落过账,本次是迟到的旧样本——**不碰基线**,CPU% = None。
///   少了这条,旧样本会按旧身份重置基线,把新代次的第一次 CPU% 也搭进去;
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
            if baseline.at > now {
                // 迟到的旧样本:已经有更晚的采样(通常是换代后的新执行)落账。
                // 原样保留,不重置也不差分——跨代差分是虚假数据。
                return (None, Some(baseline.clone()));
            }
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

// === 容器内进程明细采集（资源观测扩展）===

/// 进程角色:与内存看门狗淘汰语义对齐。
/// infra/platform = 基础设施与平台服务(看门狗永不淘汰);
/// workload = 会话命令树(看门狗的可淘汰范围)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRole {
    Infra,
    Platform,
    Workload,
}

impl ProcessRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Infra => "infra",
            Self::Platform => "platform",
            Self::Workload => "workload",
        }
    }
}

/// 单进程明细行。
#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    /// 进程组 id。仅用于在本地按 agent 进程组聚合，不对外暴露。
    pub pgrp: u32,
    pub comm: String,
    pub rss_bytes: u64,
    /// 累计 CPU 时间(秒,utime+stime)
    pub cpu_seconds: f64,
    pub role: ProcessRole,
    /// 命令行前缀(≤120 字符)
    pub args_head: String,
}

/// 按角色聚合的分组统计(字节口径)。
#[derive(Debug, Default, Clone)]
pub struct ProcessGroups {
    pub infra_rss_bytes: u64,
    pub platform_rss_bytes: u64,
    pub workload_rss_bytes: u64,
}

/// 采集容器内全部进程明细并按角色分类。
///
/// 角色语义与看门狗**淘汰范围**严格对齐(共用 `mem_watch::evictable_pids`):
/// - `workload` 会话工作负载 = 在淘汰范围内 → 可能被杀;
/// - `platform` 平台服务 / `infra` 系统其他 = 不在范围内 → 永不淘汰。
/// 进程命令行逐个读取并**脱敏**后截断到 120 字符;平台判定用未脱敏的原始
/// 命令行(脱敏会抹掉标记),原始串不外泄。
#[cfg(target_os = "linux")]
pub fn collect_container_processes() -> Vec<ProcessRow> {
    let table = process_utils::proc_scan::snapshot();
    // 淘汰范围只算一次:与看门狗选 victim 同一口径(见 mem_watch::evictable_pids)
    let evictable = process_utils::mem_watch::evictable_pids(&table);
    collect_rows(&table, &evictable)
}

/// 从已采集的进程表构建明细行（供两个入口复用，避免重复扫 /proc）。
#[cfg(target_os = "linux")]
fn collect_rows(
    table: &std::collections::HashMap<u32, process_utils::proc_scan::ProcInfo>,
    evictable: &std::collections::HashSet<u32>,
) -> Vec<ProcessRow> {
    let mut rows: Vec<ProcessRow> = Vec::with_capacity(table.len());
    let tps = procfs::ticks_per_second().max(1) as f64;
    for (pid, info) in table {
        if pid == &std::process::id() {
            continue; // 采集进程自身
        }
        let raw_args = read_cmdline_args(*pid);
        let raw_joined = raw_args.join(" ");
        // 分类用原始串:脱敏会抹掉 --token 这类标记,导致平台服务被误判为负载
        let role = classify(&info.comm, &raw_joined, evictable.contains(pid));
        let args_head = redact_cmdline(&raw_args);
        rows.push(ProcessRow {
            pid: *pid,
            ppid: info.ppid,
            pgrp: info.pgrp,
            comm: info.comm.clone(),
            rss_bytes: info.rss_bytes,
            cpu_seconds: (info.utime_ticks + info.stime_ticks) as f64 / tps,
            role,
            args_head,
        });
    }
    rows.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes));
    rows
}

#[cfg(not(target_os = "linux"))]
pub fn collect_container_processes() -> Vec<ProcessRow> {
    Vec::new()
}

/// 命令行原文(按 NUL 切分,不过滤空段之外的任何处理)。
#[cfg(target_os = "linux")]
fn read_cmdline_args(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)] // 平台替身:非 Linux 非采集路径不使用
fn read_cmdline_args(_pid: u32) -> Vec<String> {
    Vec::new()
}

/// 需要脱敏的参数名片段(大小写不敏感,匹配 flag 名或 `KEY=` 的键名)。
#[cfg(any(target_os = "linux", test))]
const SENSITIVE_NAME_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "pass",
    "pwd",
    "apikey",
    "api-key",
    "api_key",
    "credential",
    "auth",
    "bearer",
    "cookie",
    "private-key",
    "private_key",
    "access-key",
    "access_key",
    "secret-key",
    "secret_key",
    "session-token",
    "session_token",
    "signature",
    "sig",
    "key",
];

/// 一看就是凭据的值前缀(精确匹配,避免误伤普通路径/名称)。
#[cfg(any(target_os = "linux", test))]
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "eyJ", // JWT
    "sk-", // OpenAI/Anthropic 风格
    "ghp_", "gho_", "ghs_", "ghu_",   // GitHub
    "glpat-", // GitLab
    "AKIA",   // AWS access key id
    "xox",    // Slack
    "npm_",   // npm
    "ya29.",  // Google OAuth
];

/// 脱敏掩码。
#[cfg(any(target_os = "linux", test))]
const MASK: &str = "***";

/// 参数名是否敏感(去掉前导 `-` 后按片段匹配)。
#[cfg(any(target_os = "linux", test))]
fn is_sensitive_name(name: &str) -> bool {
    let n = name.trim_start_matches('-').to_ascii_lowercase();
    !n.is_empty() && SENSITIVE_NAME_MARKERS.iter().any(|m| n.contains(m))
}

/// 值本身是否像凭据(精确前缀 + 高熵串)。
///
/// 高熵判定刻意严格:必须同时含大写字母与数字、长度 ≥20、且只含 base64/hex
/// 类字符。否则 `computer-agent-runner-1743762321` 这类普通名字会被误掩，
/// 观测价值就没了——真正的高熵凭据几乎都满足大小写混合。
#[cfg(any(target_os = "linux", test))]
fn looks_like_secret(value: &str) -> bool {
    if SECRET_VALUE_PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    if value.len() < 20 {
        return false;
    }
    let charset_ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='));
    if !charset_ok {
        return false;
    }
    let has_upper = value.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    has_upper && has_digit
}

/// 脱敏后的命令行(≤120 字符,NUL 分隔转空格)。
///
/// 进程明细会经 `/computer/pod/resources` 暴露给观测端,而 `--token=xxx`
/// 这类写法在真实命令行里很常见——截断长度挡不住凭据。这里做三重处理:
/// 1. `name=value`(含 `KEY=VALUE` 环境风格):名敏感或值像凭据 → 值掩码;
/// 2. 独立敏感 flag(`--password secret`):连同**下一个**参数一起掩码;
/// 3. 任意位置的高熵值:直接掩码。
///
/// 宁多掩勿漏:多掩只是少看一个参数,漏掩是把凭据送出容器。
#[cfg(any(target_os = "linux", test))]
fn redact_cmdline(args: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            out.push(MASK.to_owned());
            mask_next = false;
            continue;
        }
        if let Some(eq) = arg.find('=') {
            let (name, value) = (&arg[..eq], &arg[eq + 1..]);
            if !value.is_empty() && (is_sensitive_name(name) || looks_like_secret(value)) {
                out.push(format!("{name}={MASK}"));
                continue;
            }
            out.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            // 独立敏感 flag:值在下一个参数里
            if is_sensitive_name(arg) {
                mask_next = true;
                out.push(arg.clone());
                continue;
            }
            // 附着形式:-pSECRET / --tokenSECRET(短 flag 只有 -p 有实际用法)
            if arg.len() > 2 && arg.starts_with("-p") && looks_like_secret(&arg[2..]) {
                out.push(format!("-p{MASK}"));
                continue;
            }
            out.push(arg.clone());
            continue;
        }
        if looks_like_secret(arg) {
            out.push(MASK.to_owned());
            continue;
        }
        out.push(arg.clone());
    }
    out.join(" ").chars().take(120).collect()
}

#[cfg(not(any(target_os = "linux", test)))]
fn redact_cmdline(_args: &[String]) -> String {
    String::new()
}

/// 角色分类(纯函数,跨平台可测)。
/// 角色分类。`evictable` = 该 pid 在**看门狗的淘汰范围内**
/// (`mem_watch::evictable_pids`,即已注册 agent 命令树的后代且未受保护)。
///
/// 顺序很重要:先判 evictable。标签必须与看门狗行为一致——标成"可淘汰"却
/// 杀不掉,或反过来把不杀的标成可淘汰,都会误导人。因此"是不是负载"只由
/// 进程树决定,comm/args 只用于区分**不会**被淘汰的进程属于哪一类。
///
/// 曾经用 comm 允许名单判 infra,但 `python3 -m websockify` 的 comm 是
/// `python3`,名单里的 "websockify" 永远命中不了,远程桌面服务被误标成
/// "可淘汰"。现在 comm 不再参与判定。
#[cfg(any(target_os = "linux", test))]
fn classify(comm: &str, args_head: &str, evictable: bool) -> ProcessRole {
    let _ = comm; // comm 不可用于身份判断(会截断),也不再用于分类
    if evictable {
        return ProcessRole::Workload;
    }
    // 与看门狗的保护清单共用同一份(单一事实源):面板标成"不淘汰"的,
    // 必须正好是看门狗真正不杀的,否则标签会与行为脱节。
    if process_utils::mem_watch::is_agent_runtime(args_head) {
        return ProcessRole::Platform;
    }
    ProcessRole::Infra
}

/// 分组聚合。
pub fn group_totals(rows: &[ProcessRow]) -> ProcessGroups {
    let mut g = ProcessGroups::default();
    for row in rows {
        match row.role {
            ProcessRole::Infra => g.infra_rss_bytes += row.rss_bytes,
            ProcessRole::Platform => g.platform_rss_bytes += row.rss_bytes,
            ProcessRole::Workload => g.workload_rss_bytes += row.rss_bytes,
        }
    }
    g
}

/// 容器内 agent 执行的进程组聚合。
///
/// 刻意不含 CPU 百分比：那需要服务端保存差分基线，而多个消费者
/// （观测面板轮询 + 平台自身查询）会互相改写基线，导致各自拿到 `None`
/// 或失真值。这里只给**累计** `cpu_seconds`，由消费者自行差分。
#[derive(Debug, Clone)]
pub struct AgentExecutionSummary {
    pub session_id: Option<String>,
    pub status: String,
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
    pub process_count: u32,
}

/// 按 agent 进程组聚合容器内进程。
///
/// 不依赖 project_id：computer 容器一次只跑一个会话，且容器内的注册表
/// 本就只含本容器的执行。用"注册表里所有 root 的 pgid"做聚合，避免调用方
/// 还得猜 project_id（实测 agent 的 project_id 与容器 project_id 并不相同）。
/// 无注册执行时返回 None。
#[cfg(target_os = "linux")]
pub fn agent_execution_summary(
    table: &std::collections::HashMap<u32, process_utils::proc_scan::ProcInfo>,
) -> Option<AgentExecutionSummary> {
    let mut session_id: Option<String> = None;
    let mut status = "Idle";
    let mut pgids = std::collections::HashSet::new();
    for entry in AGENT_REGISTRY.iter_agents() {
        let info = entry.value();
        if let Some(pgid) = info.stop_handle.as_ref().and_then(|h| h.agent_pgid()) {
            pgids.insert(pgid);
        }
        if session_id.is_none() {
            session_id = Some(info.session_id.to_string());
            // 全量匹配，不写 `_ =>`：以后新增 AgentStatus 变体时，这里
            // 必须编译失败，避免新状态被悄悄当成"Processing"。
            status = match info.status {
                // Pending = 任务已入队、等待 worker。对外仍是"忙"，与
                // GetStatus 的 busy 口径一致。
                crate::model::AgentStatus::Pending => "Processing",
                crate::model::AgentStatus::Active => "Processing",
                crate::model::AgentStatus::Idle => "Idle",
                crate::model::AgentStatus::Terminating => "Terminating",
            };
        }
    }
    if pgids.is_empty() {
        return None;
    }
    let tps = procfs::ticks_per_second().max(1) as f64;
    let mut rss_bytes: u64 = 0;
    let mut ticks: u64 = 0;
    let mut process_count: u32 = 0;
    for info in table.values() {
        if !pgids.contains(&info.pgrp) {
            continue;
        }
        rss_bytes = rss_bytes.saturating_add(info.rss_bytes);
        ticks = ticks.saturating_add(info.utime_ticks.saturating_add(info.stime_ticks));
        process_count += 1;
    }
    if process_count == 0 {
        return None;
    }
    Some(AgentExecutionSummary {
        session_id,
        status: status.to_string(),
        rss_bytes,
        cpu_seconds: ticks as f64 / tps,
        process_count,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn agent_execution_summary(
    _table: &std::collections::HashMap<u32, process_utils::proc_scan::ProcInfo>,
) -> Option<AgentExecutionSummary> {
    None
}

/// 一次 `/proc` 扫描同时产出进程明细、分组聚合与 agent 进程组聚合。
///
/// 观测端只用这个入口即可拿到"容器维度 + agent 维度"，不必再调
/// `/computer/agent/status`（那个老接口的 CPU% 是服务端差分，被外部轮询
/// 会污染基线，影响平台自身查询）。
pub fn sample_container_processes_and_agent() -> (
    Vec<ProcessRow>,
    ProcessGroups,
    Option<AgentExecutionSummary>,
) {
    #[cfg(not(target_os = "linux"))]
    {
        return (Vec::new(), ProcessGroups::default(), None);
    }
    #[cfg(target_os = "linux")]
    {
        let table = process_utils::proc_scan::snapshot();
        let evictable = process_utils::mem_watch::evictable_pids(&table);
        let rows = collect_rows(&table, &evictable);
        let groups = group_totals(&rows);
        let agent = agent_execution_summary(&table);
        (rows, groups, agent)
    }
}

/// 采集容器内进程明细与分组聚合(供 GetContainerStatus 返回)。
///
/// 非 Linux 返回空明细与零聚合(采集在容器内进行)。
pub fn sample_container_processes() -> (Vec<ProcessRow>, ProcessGroups) {
    let rows = collect_container_processes();
    let groups = group_totals(&rows);
    (rows, groups)
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
    use std::time::Duration;

    /// 最小生命周期替身:暴露指定 pgid,其余行为为空实现。
    /// 仅 Linux 门控测试使用(own_process_group)。
    #[cfg(target_os = "linux")]
    mod lifecycle_test_support {
        use std::future::Future;
        use std::pin::Pin;

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
            now - Duration::from_secs(4),
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
            now - Duration::from_secs(4),
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
            now - Duration::from_secs(4),
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
            now - Duration::from_secs(4),
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
        let (percent, next) = advance_container_cpu(next, 3_000_000, t0 + Duration::from_secs(4));
        let percent = percent.expect("container percent");
        assert!((percent - 50.0).abs() < 1.0, "percent={percent}");
        // t0+5s 回退(容器重启,usage 归零重计):None + 重置。
        let (percent, next) = advance_container_cpu(next, 100_000, t0 + Duration::from_secs(5));
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

    // ==== 换代竞态:迟到的旧样本不得动新基线 ====

    #[test]
    fn late_old_sample_does_not_reset_newer_baseline() {
        // 新执行(exec-B)已建立基线;旧执行(exec-A)的迟到样本再来,
        // 必须原样保留新基线,不能按旧身份重置。
        let t0 = Instant::now();
        advance_cpu_baseline("p-late", "exec-A", 101, 500, t0, tps());
        let t1 = t0 + Duration::from_secs(2);
        let (_, newer) = advance_cpu_baseline("p-late", "exec-B", 202, 900, t1, tps());
        let newer = newer.expect("new baseline");

        // 迟到:旧样本的 now 早于新基线落账时间
        let (percent, after) = advance_cpu_baseline(
            "p-late",
            "exec-A",
            101,
            500,
            t0 + Duration::from_millis(1),
            tps(),
        );
        assert_eq!(percent, None, "跨代不得给出 CPU%");
        let after = after.expect("baseline kept");
        assert_eq!(after.execution, "exec-B", "旧样本篡改了基线归属");
        assert_eq!(after.pgid, 202);
        assert_eq!(after.ticks, newer.ticks, "旧样本篡改了水位");
    }

    #[test]
    fn invalidate_only_removes_matching_execution() {
        // 旧采样的 None 分支只应删自己的基线,不能连坐新执行
        let t0 = Instant::now();
        advance_cpu_baseline("p-inv", "exec-A", 101, 500, t0, tps());
        advance_cpu_baseline(
            "p-inv",
            "exec-B",
            202,
            900,
            t0 + Duration::from_secs(2),
            tps(),
        );

        invalidate_baseline("p-inv", Some(("exec-A", 101)));

        let kept = CPU_BASELINES.get("p-inv").expect("新执行基线必须保留");
        assert_eq!(kept.execution, "exec-B");
        assert_eq!(kept.pgid, 202);
        drop(kept);
        CPU_BASELINES.remove("p-inv");
    }

    #[test]
    fn invalidate_without_owner_clears_everything() {
        let t0 = Instant::now();
        advance_cpu_baseline("p-inv-all", "exec-A", 101, 500, t0, tps());
        invalidate_baseline("p-inv-all", None);
        assert!(!CPU_BASELINES.contains_key("p-inv-all"));
    }

    // ==== 命令行脱敏(进程明细会经 HTTP 暴露) ====
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn redacts_equals_form_secrets() {
        assert_eq!(
            redact_cmdline(&args(&[
                "claude",
                "--api-key=sk-ant-api03-XYZ1234567890abc"
            ])),
            "claude --api-key=***"
        );
        assert_eq!(
            redact_cmdline(&args(&["app", "DB_PASSWORD=hunter2Hunter2"])),
            "app DB_PASSWORD=***"
        );
        // 值本身高熵也要掩,不管参数名
        assert_eq!(
            redact_cmdline(&args(&[
                "app",
                "--url=https://x/y",
                "AbCdEf123456GhIjKlMnOp"
            ])),
            "app --url=https://x/y ***"
        );
    }

    #[test]
    fn redacts_separate_value_flag() {
        // --password 的值在下一个参数里
        assert_eq!(
            redact_cmdline(&args(&[
                "mysql",
                "-h",
                "db",
                "--password",
                "s3cr3tPassw0rd!"
            ])),
            "mysql -h db --password ***"
        );
        // 附着形式:-pSECRET
        assert_eq!(
            redact_cmdline(&args(&["curl", "-u", "user", "-pAbCdEf123456GhIjKlMn"])),
            "curl -u user -p***"
        );
    }

    #[test]
    fn keeps_ordinary_command_lines_intact() {
        // 观测价值优先:普通命令行必须原样保留
        let normal = args(&[
            "node",
            "/usr/local/bin/nuwax-file-server",
            "start",
            "--env",
            "production",
            "--port",
            "60001",
        ]);
        assert_eq!(redact_cmdline(&normal), normal.join(" "));

        let containerish = args(&["computer-agent-runner-1743762321", "--port", "8086"]);
        assert_eq!(redact_cmdline(&containerish), containerish.join(" "));
    }

    #[test]
    fn redaction_covers_known_token_prefixes() {
        for token in [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "ghp_AbCdEf123456GhIjKlMnOpQrSt",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxb-1234567890-AbCdEfGhIjKl",
        ] {
            let out = redact_cmdline(&args(&["agent", token]));
            assert_eq!(out, "agent ***", "token {token} 必须被掩码");
        }
    }

    #[test]
    fn redaction_truncates_to_120_chars() {
        let long: Vec<String> = (0..40).map(|i| format!("arg{i:04}")).collect();
        let out = redact_cmdline(&long);
        assert!(
            out.chars().count() <= 120,
            "长度 {} 超限",
            out.chars().count()
        );
    }

    #[test]
    fn classify_uses_raw_args_not_redacted() {
        // 平台标记出现在被脱敏的参数里时,平台判定仍必须认出来
        // (脱敏只作用于输出,不作用于分类)
        let raw = args(&["node", "mcp-proxy", "--token=sk-ant-AbCdEf123456GhIjKlMn"]);
        assert_eq!(
            classify("node", &raw.join(" "), false),
            ProcessRole::Platform
        );
        let redacted = redact_cmdline(&raw);
        assert!(redacted.contains("mcp-proxy"), "脱敏不应抹掉非敏感参数");
        assert!(!redacted.contains("sk-ant-"), "凭据必须被掩");
    }

    #[test]
    fn remote_desktop_service_is_never_labelled_workload() {
        // 反例(用户实拍发现):`python3 -m websockify` 的 comm 是 `python3`,
        // 旧的 comm 允许名单里的 "websockify" 永远命中不了,于是远程桌面服务
        // 被标成"可淘汰"。现在 comm 不参与判定,树外进程一律不淘汰。
        let cmd = "python3 -m websockify --web /opt/noVNC 6080 localhost:5900 --heartbeat 30";
        assert_eq!(classify("python3", cmd, false), ProcessRole::Infra);
        // 同理:Xvnc 的桌面子进程、xfce 面板等都不该是负载
        for cmd in [
            "Xvnc :0 -geometry 1400x1050",
            "xfce4-panel",
            "fcitx5 -d --replace",
            "/usr/bin/python3 /share/system-config-printer/applet.py",
        ] {
            assert_eq!(
                classify("python3", cmd, false),
                ProcessRole::Infra,
                "树外进程必须不淘汰: {cmd}"
            );
        }
    }

    #[test]
    fn workload_label_follows_the_watchdog_scope_only() {
        // "可淘汰"只由进程树决定:同一个 websockify 命令行,在淘汰范围内就是
        // Workload(看门狗真会杀),不在就不是。标签与行为必须一致。
        let cmd = "python3 -m websockify --web /opt/noVNC 6080 localhost:5900";
        assert_eq!(classify("python3", cmd, true), ProcessRole::Workload);
        assert_eq!(classify("python3", cmd, false), ProcessRole::Infra);
        // comm 是什么都不影响:node 既可能是平台也可能是负载
        assert_eq!(
            classify("node", "node /usr/local/bin/mcp-proxy", false),
            ProcessRole::Platform
        );
        assert_eq!(
            classify("node", "node /usr/local/bin/mcp-proxy", true),
            ProcessRole::Workload
        );
    }
}

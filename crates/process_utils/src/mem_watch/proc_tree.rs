//! `/proc` 解析与候选选择。
//!
//! 本模块是**纯函数**：不读真实 `/proc`、不做 IO，输入是解析后的进程表，
//! 因此可以在任意平台（含 macOS 开发机）跑单测。

use std::collections::{HashMap, HashSet};

/// `/proc/<pid>/stat` 中我们关心的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// 进程组 id（`stat.pgrp`）。资源采集按它聚合。
    pub pgrp: u32,
    /// 内核的 `comm`，最多 15 字节，**不可用于身份判断**（会截断），仅用于日志。
    pub comm: String,
    pub rss_bytes: u64,
    /// `/proc/<pid>/stat` 第 22 字段，用于识别 PID 复用。
    pub starttime: u64,
    /// 用户态/内核态 CPU 时间（clock ticks，除以 `sysconf(SC_CLK_TCK)` 得秒）。
    pub utime_ticks: u64,
    pub stime_ticks: u64,
}

/// `)` 之后字段的下标（`state` 记为 0）。
const FIELD_PPID: usize = 1;
const FIELD_PGRP: usize = 2;
const FIELD_UTIME: usize = 11;
const FIELD_STIME: usize = 12;
const FIELD_STARTTIME: usize = 19;
const FIELD_RSS: usize = 21;
/// `/proc/stat` 的字段个数下限（rss 之后的字段不需要）。
const MIN_FIELDS: usize = FIELD_RSS + 1;

/// 解析一行 `/proc/<pid>/stat`。
///
/// 格式：`pid (comm) state ppid ...`
/// `comm` 可能包含空格和括号（甚至 `)`），所以必须按**最后一个** `)` 切分。
pub fn parse_stat(pid: u32, line: &[u8], page_size: u64) -> Option<ProcInfo> {
    let rparen = line.iter().rposition(|b| *b == b')')?;
    let lparen = line.iter().position(|b| *b == b'(')?;
    if rparen <= lparen {
        return None;
    }
    let comm = String::from_utf8_lossy(&line[lparen + 1..rparen]).into_owned();

    let tail = &line[rparen + 1..];
    let fields: Vec<&[u8]> = tail
        .split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty())
        .collect();
    if fields.len() < MIN_FIELDS {
        return None;
    }

    let parse =
        |idx: usize| -> Option<u64> { std::str::from_utf8(fields[idx]).ok()?.parse::<u64>().ok() };

    let ppid = u32::try_from(parse(FIELD_PPID)?).ok()?;
    let pgrp = u32::try_from(parse(FIELD_PGRP)?).ok()?;
    let starttime = parse(FIELD_STARTTIME)?;
    let rss_pages = parse(FIELD_RSS)?;

    Some(ProcInfo {
        pid,
        ppid,
        pgrp,
        comm,
        rss_bytes: rss_pages.saturating_mul(page_size),
        starttime,
        utime_ticks: parse(FIELD_UTIME)?,
        stime_ticks: parse(FIELD_STIME)?,
    })
}

/// 以 `root` 为起点收集所有后代（不含 `root` 自己）。
pub fn descendants(table: &HashMap<u32, ProcInfo>, root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::with_capacity(table.len());
    for info in table.values() {
        children.entry(info.ppid).or_default().push(info.pid);
    }

    let mut seen = HashSet::new();
    let mut stack: Vec<u32> = children.get(&root).cloned().unwrap_or_default();
    while let Some(pid) = stack.pop() {
        if pid == root || !seen.insert(pid) {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    seen
}

/// 被选中的淘汰目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Victim {
    pub pid: u32,
    pub rss_bytes: u64,
    pub comm: String,
}

/// 在多个会话的子树里选一个淘汰目标。
///
/// - 只考虑 `roots` 的后代（`roots` 自己永不入选）；
/// - 排除 `protected`（启动窗口快照、其它会话 root、PID 1 等）；
/// - 排除 RSS 小于 `min_rss` 的小进程，避免"为了几百 KB 杀进程"；
/// - 多个候选并列时取 RSS 最大；RSS 相同时取 pid 小的，保证结果可复现。
pub fn select_victim(
    table: &HashMap<u32, ProcInfo>,
    roots: &[u32],
    protected: &HashSet<u32>,
    min_rss: u64,
) -> Option<Victim> {
    let mut scope: HashSet<u32> = HashSet::new();
    for root in roots {
        scope.extend(descendants(table, *root));
    }

    let mut best: Option<&ProcInfo> = None;
    for pid in scope {
        if pid == 1 || roots.contains(&pid) || protected.contains(&pid) {
            continue;
        }
        let Some(info) = table.get(&pid) else {
            continue;
        };
        if info.rss_bytes < min_rss {
            continue;
        }
        best = match best {
            None => Some(info),
            Some(current) => {
                if info.rss_bytes > current.rss_bytes
                    || (info.rss_bytes == current.rss_bytes && info.pid < current.pid)
                {
                    Some(info)
                } else {
                    Some(current)
                }
            }
        };
    }

    best.map(|info| Victim {
        pid: info.pid,
        rss_bytes: info.rss_bytes,
        comm: info.comm.clone(),
    })
}

/// 淘汰目标：victim 本身 + 它的所有后代，排除 `exclude`（PID 1、root、受保护进程）。
///
/// 为什么不是只杀 victim：命令树常见 `bash -c "..."` 里再起子进程；
/// 只杀父进程会让子进程被 reparent 到 PID 1，从此脱离 `descendants(root)`
/// 的范围——内存没释放，而且再也不会被选中。
pub fn subtree_targets(
    table: &HashMap<u32, ProcInfo>,
    victim: u32,
    exclude: &HashSet<u32>,
) -> Vec<u32> {
    let mut targets = vec![victim];
    targets.extend(descendants(table, victim));
    targets.retain(|pid| *pid != 1 && !exclude.contains(pid));
    targets.sort_unstable();
    targets.dedup();
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: u64 = 4096;

    /// 构造一行 stat（字段下标与解析器一致）。
    fn stat_line(pid: u32, comm: &str, ppid: u32, rss_pages: u64, starttime: u64) -> Vec<u8> {
        let mut fields = vec!["0".to_string(); MIN_FIELDS];
        fields[0] = "S".to_string();
        fields[FIELD_PPID] = ppid.to_string();
        fields[FIELD_PGRP] = ppid.to_string();
        fields[FIELD_UTIME] = "7".to_string();
        fields[FIELD_STIME] = "3".to_string();
        fields[FIELD_STARTTIME] = starttime.to_string();
        fields[FIELD_RSS] = rss_pages.to_string();
        format!("{pid} ({comm}) {}", fields.join(" ")).into_bytes()
    }

    fn info(pid: u32, ppid: u32, rss_bytes: u64, starttime: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgrp: ppid,
            comm: format!("p{pid}"),
            rss_bytes,
            starttime,
            utime_ticks: 7,
            stime_ticks: 3,
        }
    }

    #[test]
    fn parses_basic_stat() {
        let parsed = parse_stat(42, &stat_line(42, "python3", 7, 100, 12345), PS).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.ppid, 7);
        assert_eq!(parsed.comm, "python3");
        assert_eq!(parsed.rss_bytes, 100 * PS);
        assert_eq!(parsed.starttime, 12345);
        assert_eq!(parsed.pgrp, 7);
        assert_eq!(parsed.utime_ticks, 7);
        assert_eq!(parsed.stime_ticks, 3);
    }

    #[test]
    fn parses_comm_with_spaces_and_parens() {
        // 内核允许 comm 里有空格和括号，必须按最后一个 ')' 切
        let line = stat_line(9, "my (weird) proc", 1, 5, 99);
        let parsed = parse_stat(9, &line, PS).unwrap();
        assert_eq!(parsed.comm, "my (weird) proc");
        assert_eq!(parsed.ppid, 1);
    }

    #[test]
    fn rejects_truncated_stat() {
        assert!(parse_stat(1, b"1 (x) S 0", PS).is_none());
        assert!(parse_stat(1, b"", PS).is_none());
    }

    #[test]
    fn collects_descendants_only() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 0, 0)); // root
        table.insert(11, info(11, 10, 0, 0)); // child
        table.insert(12, info(12, 11, 0, 0)); // grandchild
        table.insert(13, info(13, 1, 0, 0)); // 别的会话

        let found = descendants(&table, 10);
        assert_eq!(found, HashSet::from([11, 12]));
        assert!(!found.contains(&10), "root 不能出现在后代里");
        assert!(!found.contains(&13), "不能跨会话");
    }

    #[test]
    fn descendants_survives_cycles() {
        // 防御性：损坏的表里 ppid 成环也不能死循环
        let mut table = HashMap::new();
        table.insert(1, info(1, 2, 0, 0));
        table.insert(2, info(2, 1, 0, 0));
        drop(descendants(&table, 1));
    }

    #[test]
    fn selects_largest_and_respects_exclusions() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 900 * 1024 * 1024, 0)); // root，必须排除
        table.insert(11, info(11, 10, 800 * 1024 * 1024, 0)); // 目标
        table.insert(12, info(12, 10, 500 * 1024 * 1024, 0));
        table.insert(13, info(13, 10, 900 * 1024 * 1024, 0)); // protected
        table.insert(14, info(14, 10, 10 * 1024 * 1024, 0)); // 太小

        let protected = HashSet::from([13u32]);
        let min_rss = 64 * 1024 * 1024;
        let victim = select_victim(&table, &[10], &protected, min_rss).unwrap();

        assert_eq!(victim.pid, 11);
        assert_eq!(victim.rss_bytes, 800 * 1024 * 1024);
    }

    #[test]
    fn select_victim_returns_none_when_all_protected() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 900 * 1024 * 1024, 0));
        table.insert(11, info(11, 10, 800 * 1024 * 1024, 0));
        let protected = HashSet::from([11u32]);
        assert!(select_victim(&table, &[10], &protected, 0).is_none());
    }

    #[test]
    fn select_victim_ignores_other_sessions() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 10, 0)); // root A
        table.insert(20, info(20, 1, 10, 0)); // root B
        table.insert(21, info(21, 20, 900 * 1024 * 1024, 0)); // B 的大进程

        // 只有 A 在 registry 里，B 的进程不能被选中
        assert!(select_victim(&table, &[10], &HashSet::new(), 0).is_none());
        // 两个都在 registry 里时，才可能选到 B 的那个
        let victim = select_victim(&table, &[10, 20], &HashSet::new(), 0).unwrap();
        assert_eq!(victim.pid, 21);
    }

    #[test]
    fn subtree_targets_includes_children_but_not_excluded() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 0, 0)); // root
        table.insert(11, info(11, 10, 0, 0)); // bash（victim）
        table.insert(12, info(12, 11, 0, 0)); // python
        table.insert(13, info(13, 12, 0, 0)); // python 子进程
        table.insert(14, info(14, 11, 0, 0)); // 受保护（例如 MCP）

        let exclude = HashSet::from([14u32]);
        let targets = subtree_targets(&table, 11, &exclude);
        assert_eq!(targets, vec![11, 12, 13], "要带上后代，但不能碰受保护进程");
    }

    #[test]
    fn subtree_targets_skips_pid1() {
        let mut table = HashMap::new();
        table.insert(1, info(1, 0, 0, 0));
        table.insert(11, info(11, 10, 0, 0));
        let targets = subtree_targets(&table, 11, &HashSet::new());
        assert_eq!(targets, vec![11]);
    }

    #[test]
    fn select_victim_tie_breaks_by_pid() {
        let mut table = HashMap::new();
        table.insert(10, info(10, 1, 0, 0));
        table.insert(11, info(11, 10, 100, 0));
        table.insert(12, info(12, 10, 100, 0));
        let victim = select_victim(&table, &[10], &HashSet::new(), 0).unwrap();
        assert_eq!(victim.pid, 11);
    }
}

//! cgroup v2 内存指标：解析与 working set 计算。
//!
//! 为什么不直接用 `memory.current`：它包含 page cache。一个命令读 2GiB 文件
//! 就能把它顶到 limit 附近，但内核可以直接丢弃缓存——此时杀进程是误杀。
//! `working_set = memory.current - memory.stat.inactive_file` 与 Kubernetes
//! 驱逐使用的 `container_memory_working_set_bytes` 口径一致。

/// 容器内 cgroup v2 路径（cgroup namespace 下即本容器的 cgroup）。
pub const MEMORY_CURRENT_PATH: &str = "/sys/fs/cgroup/memory.current";
pub const MEMORY_MAX_PATH: &str = "/sys/fs/cgroup/memory.max";
pub const MEMORY_STAT_PATH: &str = "/sys/fs/cgroup/memory.stat";

/// 从 `memory.stat` 文本里取一个字段（字节）。
///
/// 格式：每行 `key value`，key 不含空格。
pub fn stat_field(stat: &str, key: &str) -> Option<u64> {
    for line in stat.lines() {
        let mut parts = line.split_ascii_whitespace();
        if parts.next() == Some(key) {
            return parts.next()?.parse().ok();
        }
    }
    None
}

/// `working_set = current - inactive_file`（饱和减法）。
///
/// `memory.stat` 里没有 `inactive_file`（异常/老内核）时，退化为 `current`，
/// 宁可多触发一次，也不要因为解析失败而完全不工作。
pub fn working_set(current: u64, memory_stat: &str) -> u64 {
    let inactive = stat_field(memory_stat, "inactive_file").unwrap_or(0);
    current.saturating_sub(inactive)
}

/// 解析 `memory.max`：`max`（无限制）返回 `None`。
pub fn parse_limit(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "\
anon 1048576
file 2097152
inactive_file 1572864
active_file 524288
shmem 4096
";

    #[test]
    fn reads_field() {
        assert_eq!(stat_field(STAT, "inactive_file"), Some(1_572_864));
        assert_eq!(stat_field(STAT, "anon"), Some(1_048_576));
        assert_eq!(stat_field(STAT, "missing"), None);
    }

    #[test]
    fn working_set_discounts_page_cache() {
        // current 8GiB、其中 4GiB 是可丢弃的 page cache → working set 只有 4GiB
        let current = 8 * 1024 * 1024 * 1024u64;
        let stat = "inactive_file 4294967296\n";
        assert_eq!(working_set(current, stat), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn working_set_never_underflows() {
        // inactive_file 比 current 大（统计口径变化/竞态）时饱和到 0
        assert_eq!(working_set(100, "inactive_file 999999\n"), 0);
    }

    #[test]
    fn working_set_falls_back_when_field_missing() {
        assert_eq!(working_set(1000, "anon 1\n"), 1000);
    }

    #[test]
    fn parses_limit() {
        assert_eq!(parse_limit("8589934592\n"), Some(8_589_934_592));
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit(" 1048576 "), Some(1_048_576));
    }
}

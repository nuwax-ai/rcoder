//! cgroup v2 内存指标：解析与 working set 计算。
//!
//! 为什么不直接用 `memory.current`：它包含 page cache。一个命令读 2GiB 文件
//! 就能把它顶到 limit 附近，但内核可以直接丢弃缓存——此时杀进程是误杀。
//! `working_set = memory.current - memory.stat.inactive_file` 与 Kubernetes
//! 驱逐使用的 `container_memory_working_set_bytes` 口径一致。
//!
//! ## 用量与限额必须同源
//!
//! 容器默认跑在自己的 cgroup 命名空间里，`/sys/fs/cgroup` 根即本容器 cgroup，
//! 直接读根路径就是本容器用量。但 **host cgroup namespace** 形态下
//! （`cgroupns=host`，部分 containerd 集群与 `--cgroupns=host` 的 Docker），
//! `/sys/fs/cgroup` 暴露的是**宿主整棵 cgroup 树**：根路径的 `memory.current`
//! 是**整机**用量。此时若限额来自 env 覆盖（容器限额），就会拿节点用量和
//! 容器限额比大小——节点一忙就误杀 agent 进程。
//!
//! 因此 [`CgroupProbe`] 先从 `/proc/self/cgroup` 解析本进程在宿主树里的相对
//! 路径，三个文件都从**同一组**读；限额来自 env 覆盖时还要和实测 `memory.max`
//! 对账，不一致就拒绝淘汰并说明原因。

use std::path::PathBuf;

/// cgroup v2 文件系统的挂载点（容器内视角）。
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// 容器内 cgroup v2 路径（cgroup 命名空间根即本容器 cgroup 时的默认值）。
pub const MEMORY_CURRENT_PATH: &str = "/sys/fs/cgroup/memory.current";
pub const MEMORY_MAX_PATH: &str = "/sys/fs/cgroup/memory.max";
pub const MEMORY_STAT_PATH: &str = "/sys/fs/cgroup/memory.stat";

/// 本进程在宿主 cgroup 树中的相对路径（v2 行 `0::/...`）。
///
/// - `Some("")`：cgroup 命名空间根即本容器 cgroup（Docker/k8s 默认）；
/// - `Some("/kubepods/...")`：host cgroup namespace，需按此路径找本容器组；
/// - `None`：读不到 `/proc/self/cgroup`，无法确认用量归属。
pub fn self_relpath() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    // v2 统一层级行形如 `0::/path`；v1 混合形态取不到 v2 行时按无法确认处理。
    let line = raw.lines().find(|l| l.starts_with("0::"))?;
    let path = line.get(3..)?.trim();
    if path.is_empty() {
        return Some(String::new());
    }
    // 归一化：去掉末尾 '/'，保留前导 '/'
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        Some(String::new())
    } else {
        Some(format!("/{}", trimmed.trim_start_matches('/')))
    }
}

/// 按相对路径拼出 cgroup 文件路径。
fn join(relpath: &str, leaf: &str) -> PathBuf {
    if relpath.is_empty() {
        PathBuf::from(format!("{CGROUP_ROOT}/{leaf}"))
    } else {
        PathBuf::from(format!("{CGROUP_ROOT}{relpath}/{leaf}"))
    }
}

/// 一次解析出的 cgroup 定位与实测限额。
///
/// 采样循环启动时解析一次：cgroup 归属在进程生命周期内不变，
/// 每 tick 重读反而会把瞬时错误变成行为抖动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupProbe {
    relpath: String,
    max: Option<u64>,
    /// 定位失败的原因（`relpath` 无法确认时非空）
    unresolved: Option<&'static str>,
}

impl CgroupProbe {
    /// 解析本进程所属 cgroup 并读一次 `memory.max`。
    pub fn resolve() -> Self {
        match self_relpath() {
            Some(relpath) => {
                let max = read_u64(&join(&relpath, "memory.max"));
                Self {
                    relpath,
                    max,
                    unresolved: None,
                }
            }
            None => Self {
                relpath: String::new(),
                max: None,
                unresolved: Some("cannot read /proc/self/cgroup v2 line"),
            },
        }
    }

    /// 是否定位到"命名空间根即本容器 cgroup"（最良形态）。
    pub fn is_namespace_root(&self) -> bool {
        self.relpath.is_empty()
    }

    /// 相对路径（空 = 命名空间根）。
    pub fn relpath(&self) -> &str {
        &self.relpath
    }

    /// 实测 `memory.max`；`max`（无限制）或读不到均为 `None`。
    pub fn max(&self) -> Option<u64> {
        self.max
    }

    /// 定位失败原因。
    pub fn unresolved(&self) -> Option<&'static str> {
        self.unresolved
    }

    pub fn current_path(&self) -> PathBuf {
        join(&self.relpath, "memory.current")
    }

    pub fn stat_path(&self) -> PathBuf {
        join(&self.relpath, "memory.stat")
    }

    /// 用量与限额是否同源可确认。
    ///
    /// env 覆盖的限额必须和本组的实测 `memory.max` 一致：不一致说明限额来自
    /// 别处（典型：host cgroupns 下拿节点用量比容器限额），此时继续淘汰就是
    /// 按错误口径杀人。
    pub fn confirms(&self, limit: u64) -> bool {
        self.unresolved.is_none() && self.max == Some(limit)
    }
}

/// 读一个 cgroup 文件为 u64；`max` 或读不到返回 `None`。
pub fn read_u64(path: &std::path::Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_limit(&raw)
}

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

    #[test]
    fn relpath_normalizes() {
        // 纯函数部分：归一化规则（不读真实 /proc）
        fn norm(path: &str) -> String {
            let trimmed = path.trim_end_matches('/');
            if trimmed.is_empty() {
                String::new()
            } else {
                format!("/{}", trimmed.trim_start_matches('/'))
            }
        }
        assert_eq!(norm("/"), "");
        assert_eq!(norm(""), "");
        assert_eq!(norm("/kubepods/pod-1"), "/kubepods/pod-1");
        assert_eq!(norm("kubepods/pod-1/"), "/kubepods/pod-1");
    }

    #[test]
    fn probe_confirms_only_when_max_matches_limit() {
        let root = CgroupProbe {
            relpath: String::new(),
            max: Some(4 << 30),
            unresolved: None,
        };
        assert!(root.is_namespace_root());
        assert!(root.confirms(4 << 30), "实测 max 与限额一致才放行");
        assert!(
            !root.confirms(8 << 30),
            "限额来自别处时必须拒绝：这是 host cgroupns 误杀的防线"
        );
        assert_eq!(root.current_path().to_str(), Some(MEMORY_CURRENT_PATH));
        assert_eq!(root.stat_path().to_str(), Some(MEMORY_STAT_PATH));

        let nested = CgroupProbe {
            relpath: "/kubepods/pod-1".to_owned(),
            max: Some(512 << 20),
            unresolved: None,
        };
        assert!(!nested.is_namespace_root());
        assert_eq!(nested.relpath(), "/kubepods/pod-1");
        assert!(nested.confirms(512 << 20));
        assert!(
            nested
                .current_path()
                .to_str()
                .unwrap()
                .ends_with("/kubepods/pod-1/memory.current"),
            "用量必须从本组读，不能读宿主根"
        );

        let broken = CgroupProbe {
            relpath: String::new(),
            max: None,
            unresolved: Some("boom"),
        };
        assert!(!broken.confirms(4 << 30), "定位失败一律拒绝淘汰");
        assert_eq!(broken.unresolved(), Some("boom"));
    }

    #[test]
    fn probe_resolves_on_real_linux() {
        // Linux 容器内必须能定位；macOS 开发机读不到 /proc，跳过
        if !cfg!(target_os = "linux") {
            return;
        }
        let probe = CgroupProbe::resolve();
        assert!(probe.unresolved().is_none(), "Linux 上应能读到 0:: 行");
    }
}

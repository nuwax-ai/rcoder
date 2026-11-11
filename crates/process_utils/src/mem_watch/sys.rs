//! 操作系统交互的最小封装。
//!
//! 非 Linux 平台全部返回空/`None`：本模块只保证 crate 在 macOS 上能编译，
//! 真正的看门狗只在 Linux 容器内运行。

use std::collections::HashMap;
use std::path::PathBuf;

use super::proc_tree::ProcInfo;

#[cfg(target_os = "linux")]
pub fn page_size() -> u64 {
    // rustix 的安全封装（内部处理 sysconf/auxv）
    rustix::param::page_size() as u64
}

#[cfg(not(target_os = "linux"))]
#[cfg_attr(not(test), allow(dead_code))]
pub fn page_size() -> u64 {
    4096
}

/// 读取单个进程的 stat。
#[cfg(target_os = "linux")]
pub fn read_pid(pid: u32) -> Option<ProcInfo> {
    let pid = i32::try_from(pid).ok()?;
    let stat = procfs::process::Process::new(pid).ok()?.stat().ok()?;
    Some(into_proc_info(stat))
}

#[cfg(not(target_os = "linux"))]
pub fn read_pid(_pid: u32) -> Option<ProcInfo> {
    None
}

/// 读取全部进程（失败的单条跳过）。
#[cfg(target_os = "linux")]
pub fn read_all() -> HashMap<u32, ProcInfo> {
    let mut out = HashMap::new();
    let Ok(all) = procfs::process::all_processes() else {
        return out;
    };
    for process in all.flatten() {
        // 单条失败(进程刚退出/权限不足)跳过,不能中断整表扫描。
        let Ok(stat) = process.stat() else {
            continue;
        };
        let info = into_proc_info(stat);
        out.insert(info.pid, info);
    }
    out
}

/// `procfs` 的 stat → [`ProcInfo`]。
///
/// 用 `procfs` 而不是自己按字段下标切 `/proc/<pid>/stat`：那是内核 ABI，
/// 但字段解析、`comm` 里可能出现的空格与括号、各字段类型都有现成实现，
/// 没必要自己维护（实测 203 进程全表扫描两者差 1.1x，可忽略）。
///
/// `rss` 由内核以**页**为单位给出，这里换算成字节；页大小随架构变化
/// （arm64 可能是 16KiB/64KiB），必须运行时查询，不能写死 4096。
#[cfg(target_os = "linux")]
fn into_proc_info(stat: procfs::process::Stat) -> ProcInfo {
    let page = page_size();
    ProcInfo {
        pid: stat.pid as u32,
        ppid: stat.ppid as u32,
        pgrp: stat.pgrp as u32,
        comm: stat.comm,
        rss_bytes: stat.rss.saturating_mul(page),
        starttime: stat.starttime,
        utime_ticks: stat.utime,
        stime_ticks: stat.stime,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn read_all() -> HashMap<u32, ProcInfo> {
    HashMap::new()
}

/// 进程可执行文件路径（已解析符号链接）。权限不足或已退出返回 `None`。
#[cfg(target_os = "linux")]
pub fn read_exe(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
pub fn read_exe(_pid: u32) -> Option<PathBuf> {
    None
}

/// 命令行参数（已按 NUL 切分）。
#[cfg(target_os = "linux")]
pub fn read_cmdline(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn read_cmdline(_pid: u32) -> Vec<String> {
    Vec::new()
}

/// 当前 cgroup 的内存用量（含 page cache）。
///
/// 必须传 [`cgroup::CgroupProbe`]：host cgroup namespace 下根路径是宿主整机
/// 用量，拿它和容器限额比较会误杀（见 `cgroup` 模块文档）。
pub fn read_memory_current(probe: &super::cgroup::CgroupProbe) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        super::cgroup::read_u64(&probe.current_path())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = probe;
        None
    }
}

/// `memory.max`；`max`（无限制）返回 `None`。
pub fn read_memory_max() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string(super::cgroup::MEMORY_MAX_PATH).ok()?;
        super::cgroup::parse_limit(&raw)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `memory.stat` 原文（同样按 probe 定位的组读）。
pub fn read_memory_stat(probe: &super::cgroup::CgroupProbe) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(probe.stat_path()).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = probe;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_sane() {
        let ps = page_size();
        assert!(
            ps == 4096 || ps == 16384 || ps == 65536,
            "unexpected page size {ps}"
        );
    }
}

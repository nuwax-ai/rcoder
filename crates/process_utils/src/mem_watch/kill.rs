//! 进程终止：用 `pidfd`（rustix 的安全封装，免疫 PID 复用）。
//!
//! 项目红线禁止 `unsafe`，所以这里不用 `libc::syscall` 裸调；
//! `rustix::process::pidfd_open` / `pidfd_send_signal` 是安全封装。
//!
//! ## 为什么还要复核 starttime
//!
//! `pidfd_open(pid)` 绑定的是**调用那一刻**该 PID 对应的进程。从扫表到打开
//! pidfd 之间有真实窗口：原进程退出且 PID 被复用后，pidfd 会绑定一个无关的
//! 新进程——pidfd 只保证"不会中途换目标"，不保证"目标就是当初那个"。
//! 因此每个目标都携带快照里的 `starttime`，打开 pidfd 后、发信号前再读一次
//! `/proc/<pid>/stat` 复核：不一致说明 PID 已易主，跳过该目标。
//!
//! ## 为什么要逐目标收集结果
//!
//! 只报告首个 TERM 会漏掉后代的失败：父进程放行、子进程被 LSM 拒绝时，
//! 调用方看到成功就不会降级停用——"以为在工作其实杀不掉"是最坏的失效形态。

use std::time::Duration;

use super::proc_tree::KillTarget;

/// 单个目标的信号错误。
///
/// `std::io::Error` 不实现 `PartialEq`，这里拆成 kind + 详情文本，
/// 让报告可比较、可断言，同时保留日志需要的原始信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalError {
    pub pid: u32,
    pub kind: std::io::ErrorKind,
    /// 原始错误的 Display 文本（仅供日志）
    pub detail: String,
}

/// 逐目标终止结果。
///
/// 区分三类终态，让调用方能判断"杀干净了"还是"有目标没杀动"：
/// - `term_ok` / `killed`：信号确实送达；
/// - `gone`：目标已退出（ESRCH）或身份不符（PID 易主），无需再杀；
/// - `denied` / `failed`：LSM 拒绝或其它错误，必须上报。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminateReport {
    /// SIGTERM 成功送达的目标数
    pub term_ok: usize,
    /// grace 后仍需 SIGKILL 并成功送达的目标数
    pub killed: usize,
    /// 已退出或身份不符而跳过的目标数
    pub gone: usize,
    /// 身份不符（快照 starttime 与现状不同）而跳过的目标
    pub reused: Vec<u32>,
    /// 被拒绝的目标，典型是 LSM(EPERM)
    pub denied: Vec<SignalError>,
    /// 其它错误的目标
    pub failed: Vec<SignalError>,
}

impl TerminateReport {
    /// 没有任何信号被拒绝/失败。
    pub fn is_clean(&self) -> bool {
        self.denied.is_empty() && self.failed.is_empty()
    }

    /// 首个 LSM 拒绝（调用方据此降级停用看门狗）。
    pub fn denied(&self) -> Option<&SignalError> {
        self.denied
            .iter()
            .find(|e| e.kind == std::io::ErrorKind::PermissionDenied)
    }

    /// 供日志的人类可读汇总。
    pub fn summary(&self) -> String {
        format!(
            "term_ok={} killed={} gone={} reused={:?} denied={} failed={}",
            self.term_ok,
            self.killed,
            self.gone,
            self.reused,
            self.denied.len(),
            self.failed.len()
        )
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Duration, KillTarget, TerminateReport};
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

    /// 读 `/proc/<pid>/stat` 的 starttime（第 22 字段）。不存在返回 `None`。
    fn current_starttime(pid: u32) -> Option<u64> {
        let raw = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        super::super::proc_tree::parse_stat(pid, &raw, super::super::sys::page_size())
            .map(|info| info.starttime)
    }

    fn classify(err: std::io::Error, pid: u32, report: &mut TerminateReport) {
        let entry = SignalError {
            pid,
            kind: err.kind(),
            detail: err.to_string(),
        };
        match entry.kind {
            // 目标已退出：不是失败
            std::io::ErrorKind::NotFound => report.gone += 1,
            std::io::ErrorKind::PermissionDenied => report.denied.push(entry),
            _ => report.failed.push(entry),
        }
    }

    pub(super) async fn terminate_many(targets: &[KillTarget], grace: Duration) -> TerminateReport {
        let mut report = TerminateReport::default();
        if targets.is_empty() {
            return report;
        }

        // (pidfd, pid) 成对保存：pid 仅用于错误归属，不发信号用它。
        let mut handles: Vec<(rustix::fd::OwnedFd, u32)> = Vec::with_capacity(targets.len());
        for target in targets {
            // 打开前先复核：快照之后进程已换人就直接放弃，连 pidfd 都不开。
            match current_starttime(target.pid) {
                Some(now) if now == target.starttime => {}
                Some(_) => {
                    report.reused.push(target.pid);
                    report.gone += 1;
                    continue;
                }
                // 读不到 stat：进程已退出
                None => {
                    report.gone += 1;
                    continue;
                }
            }
            let Some(pid) = Pid::from_raw(target.pid as i32) else {
                report.gone += 1;
                continue;
            };
            let fd = match pidfd_open(pid, PidfdFlags::empty()) {
                Ok(fd) => fd,
                Err(e) => {
                    // ESRCH:打开前那一刻退出了;其余(含 EPERM)按失败上报
                    let err = std::io::Error::from(e);
                    classify(err, target.pid, &mut report);
                    continue;
                }
            };
            // 打开后再复核一次：这段窗口里仍可能退出并被复用。
            match current_starttime(target.pid) {
                Some(now) if now == target.starttime => handles.push((fd, target.pid)),
                _ => {
                    report.reused.push(target.pid);
                    report.gone += 1;
                }
            }
        }

        if handles.is_empty() {
            return report;
        }

        // 逐个发 TERM：任一目标被拒绝都要记下，不能只记第一个。
        for (fd, pid) in &handles {
            match pidfd_send_signal(fd, Signal::TERM) {
                Ok(()) => report.term_ok += 1,
                Err(e) => classify(std::io::Error::from(e), *pid, &mut report),
            }
        }

        tokio::time::sleep(grace).await;

        // 逐个补 KILL：TERM 期间退出的目标会 ESRCH，按 gone 计。
        for (fd, pid) in &handles {
            match pidfd_send_signal(fd, Signal::KILL) {
                Ok(()) => report.killed += 1,
                Err(e) => classify(std::io::Error::from(e), *pid, &mut report),
            }
        }
        // OwnedFd 离开作用域自动 close
        report
    }
}

/// 对一组进程执行 SIGTERM → `grace` → SIGKILL。
///
/// 传整棵子树而不只是 victim：命令树里 `bash -c "..."` 的子进程在父进程死后
/// 会被 reparent 到 PID 1，从此脱离 `descendants(root)`，内存不会被释放。
///
/// 每个目标带快照 `starttime`，发信号前复核身份，避免 PID 复用后误杀无关进程。
///
/// 返回 [`TerminateReport`]：`denied()` 非空 ⇒ LSM（AppArmor/SELinux）拒绝发信号，
/// 调用方应降级停用（看门狗"以为在工作其实杀不掉"是最坏的失效形态）；
/// `gone` 里的目标已退出，不算失败。
///
/// 非 Linux 平台恒返回空报告（看门狗只在 Linux 容器内运行）。
pub async fn terminate_many(targets: &[KillTarget], grace: Duration) -> TerminateReport {
    #[cfg(target_os = "linux")]
    {
        imp::terminate_many(targets, grace).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (targets, grace);
        TerminateReport::default()
    }
}

/// 单个进程的便捷封装。
pub async fn terminate(pid: u32, starttime: u64, grace: Duration) -> TerminateReport {
    terminate_many(&[KillTarget { pid, starttime }], grace).await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn target_of(child: &tokio::process::Child) -> KillTarget {
        let pid = child.id().expect("pid");
        let raw = std::fs::read(format!("/proc/{pid}/stat")).expect("stat");
        let starttime =
            crate::mem_watch::proc_tree::parse_stat(pid, &raw, crate::mem_watch::sys::page_size())
                .expect("parse")
                .starttime;
        KillTarget { pid, starttime }
    }

    #[tokio::test]
    async fn terminates_a_real_child() {
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .spawn()
            .expect("spawn sleep");

        let report = terminate_many(&[target_of(&child)], Duration::from_millis(200)).await;

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child should exit")
            .expect("wait ok");
        assert!(!status.success());
        assert!(report.is_clean(), "正常终止不应有拒绝/失败: {report:?}");
        assert!(report.term_ok >= 1, "至少一个 TERM 送达: {report:?}");
    }

    #[tokio::test]
    async fn missing_process_is_not_an_error() {
        // 一个几乎不可能存在的 pid：打开前复核就发现不存在 → gone，不算失败
        let report = terminate_many(
            &[KillTarget {
                pid: 4_000_000,
                starttime: 1,
            }],
            Duration::from_millis(10),
        )
        .await;
        assert!(report.is_clean(), "目标已退出按成功处理: {report:?}");
        assert_eq!(report.gone, 1, "应记为 gone: {report:?}");
    }

    #[tokio::test]
    async fn stale_starttime_is_skipped_not_signalled() {
        // 反例：PID 已易主（starttime 不匹配）→ 必须跳过，绝不能发信号。
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .spawn()
            .expect("spawn sleep");
        let real = target_of(&child);

        let report = terminate_many(
            &[KillTarget {
                pid: real.pid,
                starttime: real.starttime.wrapping_add(1_000_000),
            }],
            Duration::from_millis(50),
        )
        .await;

        assert!(
            report.reused.contains(&real.pid),
            "应识别为 PID 易主: {report:?}"
        );
        assert_eq!(report.term_ok, 0, "身份不符绝不能发信号: {report:?}");
        assert!(report.is_clean(), "跳过不是失败: {report:?}");

        // 进程必须还活着
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "身份不符的目标不应被终止"
        );
        drop(child.kill());
        drop(child.wait());
    }

    #[tokio::test]
    async fn empty_targets_is_clean() {
        let report = terminate_many(&[], Duration::from_millis(1)).await;
        assert!(report.is_clean());
        assert_eq!(report.term_ok, 0);
    }

    #[test]
    fn report_summary_and_denied() {
        let mut report = TerminateReport::default();
        assert!(report.is_clean());
        assert!(report.denied().is_none());

        report.denied.push(SignalError {
            pid: 42,
            kind: std::io::ErrorKind::PermissionDenied,
            detail: "EPERM".to_owned(),
        });
        assert!(!report.is_clean());
        assert!(report.denied().is_some(), "EPERM 必须能被调用方识别");
        assert!(report.summary().contains("denied=1"));

        // 其它 kind 的失败不该被误判成 LSM 拒绝（不会触发降级停用）
        let mut other = TerminateReport::default();
        other.failed.push(SignalError {
            pid: 7,
            kind: std::io::ErrorKind::Other,
            detail: "boom".to_owned(),
        });
        assert!(!other.is_clean());
        assert!(other.denied().is_none(), "非 EPERM 不触发降级");
    }

    #[test]
    fn report_is_comparable() {
        // PartialEq 让测试能直接比较整份报告
        let a = TerminateReport {
            gone: 1,
            ..Default::default()
        };
        let b = TerminateReport {
            gone: 1,
            ..Default::default()
        };
        assert_eq!(a, b);
    }
}

//! Process-space epoch: the identity of the OS boot / PID namespace that a
//! generation ran in. A changed epoch, observed while the generation lock is
//! free, is positive local evidence that every process of that era is gone:
//! a host reboot destroys all processes, and a container restart recreates
//! the whole PID namespace. It never proves anything about processes in
//! OTHER containers or pods — callers must scope it accordingly.

/// Process-space identity, or `None` when no reliable evidence is available.
/// Wall-clock time is deliberately excluded: clock corrections do not kill
/// processes and therefore cannot authorize retirement.
pub(crate) fn current() -> Option<String> {
    if let Some(value) = test_override() {
        return value;
    }
    platform_epoch()
}

/// Positive evidence of replacement. Unknown/legacy formats are not evidence.
/// Windows exposes monotonic uptime: a decrease proves a reboot, while an
/// increase is inconclusive (the new boot may already have a longer uptime).
pub(crate) fn proves_replacement(recorded: &str, current: &str) -> bool {
    match (parse(recorded), parse(current)) {
        (Some(Epoch::Pid1(a, x)), Some(Epoch::Pid1(b, y))) => a != b || x != y,
        (Some(Epoch::MacBoot(a)), Some(Epoch::MacBoot(b))) => a != b,
        (Some(Epoch::WindowsUptime(a)), Some(Epoch::WindowsUptime(b))) => b < a,
        _ => false,
    }
}

enum Epoch {
    Pid1(uuid::Uuid, u64),
    MacBoot(uuid::Uuid),
    WindowsUptime(u64),
}

/// Equality must describe a known PID namespace/boot, not two opaque strings.
/// Windows uptime is not a stable namespace identity for PID-only inspection.
pub(crate) fn same_process_space(recorded: &str, current: &str) -> bool {
    match (parse(recorded), parse(current)) {
        (Some(Epoch::Pid1(a, x)), Some(Epoch::Pid1(b, y))) => a == b && x == y,
        (Some(Epoch::MacBoot(a)), Some(Epoch::MacBoot(b))) => a == b,
        // GetTickCount64 在同一次开机内单调不减：current >= recorded 即
        // 同一启动会话（reboot 会归零，由 proves_replacement 的 b < a
        // 捕获）。这不是完整的启动身份（新一次开机 uptime 更长时会误判
        // 同次开机）——Windows 侧由"signaled 句柄观察 + worker 创建身份
        // sidecar（RV06）"补足：uptime 只作前置过滤，误判方向是保守阻塞
        // 清理而非误放行。表述保持诚实：不是 PID namespace 身份。
        (Some(Epoch::WindowsUptime(a)), Some(Epoch::WindowsUptime(b))) => b >= a,
        _ => false,
    }
}

fn parse(value: &str) -> Option<Epoch> {
    let (kind, payload) = value.split_once(':')?;
    match kind {
        "pid1" => {
            let (boot, ticks) = payload.split_once(':')?;
            let boot = uuid::Uuid::parse_str(boot).ok()?;
            (!boot.is_nil()).then_some(Epoch::Pid1(boot, ticks.parse().ok()?))
        }
        "mac-boot" => {
            let boot = uuid::Uuid::parse_str(payload).ok()?;
            (!boot.is_nil()).then_some(Epoch::MacBoot(boot))
        }
        "win-uptime-ms" => Some(Epoch::WindowsUptime(payload.parse().ok()?)),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn platform_epoch() -> Option<String> {
    // /proc/1/stat: `pid (comm) state ... starttime(22) ...`. comm may contain
    // spaces and ')'; everything before the last ')' belongs to it.
    let stat = std::fs::read_to_string("/proc/1/stat").ok()?;
    let tail = stat.rsplit_once(')')?.1;
    // tail fields start at overall field 3; starttime is field 22 → index 19.
    let starttime = tail.split_whitespace().nth(19)?;
    let ticks: u64 = starttime.parse().ok()?;
    // The start time alone is nearly constant across host reboots (~15 ticks
    // on every boot); the boot UUID is what actually identifies the OS
    // session. Together they also distinguish container restarts (same boot
    // UUID, new PID-1 start time).
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()?
        .trim()
        .to_owned();
    Some(format!("pid1:{boot_id}:{ticks}"))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // FFI: sysctlbyname reads the kernel's stable boot-session UUID.
fn platform_epoch() -> Option<String> {
    let mut value = [0u8; 64];
    let mut length = value.len();
    // SAFETY: the named read-only sysctl writes at most `length` bytes into
    // the live buffer. No write/new-value pointer is supplied.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            value.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length > value.len() {
        return None;
    }
    let text = std::str::from_utf8(&value[..length])
        .ok()?
        .trim_end_matches('\0');
    let id = uuid::Uuid::parse_str(text).ok()?;
    (!id.is_nil()).then(|| format!("mac-boot:{id}"))
}

#[cfg(windows)]
#[allow(unsafe_code)] // FFI: read-only Win32 uptime, no pointers or wall-clock arithmetic.
fn platform_epoch() -> Option<String> {
    // A clock step cannot change GetTickCount64. Do not infer a reboot from
    // now - uptime; that used to falsely retire live descendants after NTP.
    let ticks = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
    Some(format!("win-uptime-ms:{ticks}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_epoch() -> Option<String> {
    None
}

#[cfg(test)]
thread_local! {
    // Synchronous unit tests must not override another test's platform reads.
    // Outer None = real platform; inner None = simulated unavailable evidence.
    static TEST_OVERRIDE: std::cell::RefCell<Option<(u32, Option<String>)>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Scoped to the calling test thread, including nested overrides. Not Send:
/// dropping on a different thread would fail to restore the original reader.
#[cfg(test)]
pub(crate) struct EpochGuard {
    previous: Option<(u32, Option<String>)>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[cfg(test)]
impl EpochGuard {
    pub(crate) fn new(value: Option<String>) -> Self {
        Self {
            previous: TEST_OVERRIDE.replace(Some((std::process::id(), value))),
            _thread_bound: std::marker::PhantomData,
        }
    }
}
#[cfg(test)]
impl Drop for EpochGuard {
    fn drop(&mut self) {
        TEST_OVERRIDE.set(self.previous.take());
    }
}

#[cfg(test)]
fn test_override() -> Option<Option<String>> {
    TEST_OVERRIDE.with_borrow(|slot| match slot {
        Some((pid, value)) if *pid == std::process::id() => Some(value.clone()),
        _ => None,
    })
}

#[cfg(not(test))]
fn test_override() -> Option<Option<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_override_is_nested_and_thread_local() {
        assert!(test_override().is_none());
        {
            let _outer = EpochGuard::new(Some("test-epoch".into()));
            {
                let _inner = EpochGuard::new(None);
                assert!(current().is_none());
            }
            assert_eq!(current().as_deref(), Some("test-epoch"));
            std::thread::spawn(|| assert!(test_override().is_none()))
                .join()
                .unwrap();
        }
        assert!(test_override().is_none());
    }

    #[test]
    fn only_valid_process_space_evidence_authorizes_retirement() {
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        assert!(!proves_replacement(
            &format!("pid1:{a}:15"),
            &format!("pid1:{a}:15")
        ));
        assert!(proves_replacement(
            &format!("pid1:{a}:15"),
            &format!("pid1:{b}:15")
        ));
        assert!(proves_replacement(
            &format!("pid1:{a}:15"),
            &format!("pid1:{a}:16")
        ));
        assert!(proves_replacement(
            &format!("mac-boot:{a}"),
            &format!("mac-boot:{b}")
        ));
        for old in [
            "garbage",
            "boot:1000",
            "pid1::15",
            "pid1:not-a-uuid:15",
            "pid1:1:-1",
        ] {
            assert!(!proves_replacement(old, &format!("pid1:{a}:15")), "{old}");
        }
        assert!(!proves_replacement("boot:1000", "boot:999999"));
        assert!(!proves_replacement(
            &format!("pid1:{a}:15"),
            &format!("mac-boot:{b}")
        ));
        assert!(!proves_replacement(
            "win-uptime-ms:1000",
            "win-uptime-ms:2000"
        ));
        assert!(proves_replacement(
            "win-uptime-ms:2000",
            "win-uptime-ms:1000"
        ));
    }

    #[test]
    fn real_epoch_is_readable_and_does_not_report_spurious_reboots() {
        let first = current().expect("supported platform must expose process-space evidence");
        let second = current().expect("second reading must also succeed");
        assert!(parse(&first).is_some(), "{first}");
        assert!(!proves_replacement(&first, &second), "{first} vs {second}");
    }
}

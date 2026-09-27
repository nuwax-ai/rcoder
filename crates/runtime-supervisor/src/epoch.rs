//! Process-space epoch: the identity of the OS boot / PID namespace that a
//! generation ran in. A changed epoch, observed while the generation lock is
//! free, is positive local evidence that every process of that era is gone:
//! a host reboot destroys all processes, and a container restart recreates
//! the whole PID namespace. It never proves anything about processes in
//! OTHER containers or pods — callers must scope it accordingly.

#[cfg(any(test, feature = "test-support"))]
use std::sync::OnceLock;

/// Stable identity of the current process space, or `None` when the platform
/// does not expose one (retirement then stays conservative).
///
/// Format: `"<kind>:<integer>"`. Linux uses PID 1's start time in clock ticks
/// (constant within one PID namespace, changes on container restart and host
/// reboot). macOS/Windows use the computed boot wall time; small clock
/// adjustments are tolerated by [`matches`] instead of breaking equality.
pub(crate) fn current() -> Option<String> {
    if let Some(value) = test_override() {
        return value;
    }
    platform_epoch()
}

/// Same-boot comparison. The Linux reading pairs the host boot UUID with PID
/// 1's start time and must match exactly (the start time alone is nearly
/// constant across reboots — both boots read ~15 ticks — so it can never
/// identify a reboot by itself). Boot wall-time derivations on other platforms
/// can slew slightly within one session, so nearby values count as same boot.
/// Malformed values never match (stay conservative).
pub(crate) fn matches(recorded: &str, current: &str) -> bool {
    let Some((recorded_kind, recorded)) = split(recorded) else {
        return false;
    };
    let Some((current_kind, current)) = split(current) else {
        return false;
    };
    recorded_kind == current_kind
        && match (recorded, current) {
            (Payload::Pid1(boot_a, ticks_a), Payload::Pid1(boot_b, ticks_b)) => {
                boot_a == boot_b && ticks_a == ticks_b
            }
            (Payload::Boot(a), Payload::Boot(b)) => a.abs_diff(b) <= 2_000,
            _ => false,
        }
}

enum Payload<'a> {
    /// "<boot-uuid>:<start-time ticks>" — both must match exactly.
    Pid1(&'a str, i128),
    /// Boot wall-time derivation with comparison tolerance.
    Boot(i128),
}

fn split<'a>(value: &'a str) -> Option<(&'a str, Payload<'a>)> {
    let (kind, payload) = value.split_once(':')?;
    if kind == "pid1" {
        let (boot, ticks) = payload.split_once(':')?;
        return Some((kind, Payload::Pid1(boot, ticks.parse().ok()?)));
    }
    Some((kind, Payload::Boot(payload.parse().ok()?)))
}

#[cfg(target_os = "linux")]
fn platform_epoch() -> Option<String> {
    // /proc/1/stat: `pid (comm) state ... starttime(22) ...`. comm may contain
    // spaces and ')'; everything before the last ')' belongs to it.
    let stat = std::fs::read_to_string("/proc/1/stat").ok()?;
    let tail = stat.rsplit_once(')')?.1;
    // tail fields start at overall field 3; starttime is field 22 → index 19.
    let starttime = tail.split_whitespace().nth(19)?;
    let ticks: i128 = starttime.parse().ok()?;
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
#[allow(unsafe_code)] // FFI 调用 C 的 sysctl（CLAUDE.md 明确允许的 FFI 例外）
fn platform_epoch() -> Option<String> {
    // kern.boottime 无安全 std API 可读。tv_usec 会随时钟微调漂移，只取整秒
    // 并靠 [`matches`] 容差比较。
    let mut name = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut length = size_of::<libc::timeval>() as libc::size_t;
    let result = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            2,
            std::ptr::from_mut(&mut value).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0).then(|| format!("boot:{}", value.tv_sec))
}

#[cfg(windows)]
#[allow(unsafe_code)] // FFI 调用 Win32（CLAUDE.md 明确允许的 FFI 例外）
fn platform_epoch() -> Option<String> {
    // 引导会话身份无安全 std API。boot ≈ now − uptime；休眠/时钟步进只会造成
    // 无害的同向漂移（误判为换代 ⇒ 收束一个本就已死的代次），真实重启必然大幅偏离。
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount64};
    let mut now = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let ticks;
    unsafe {
        GetSystemTimeAsFileTime(&mut now);
        ticks = GetTickCount64();
    }
    let now_100ns = (u64::from(now.dwHighDateTime) << 32) | u64::from(now.dwLowDateTime);
    let boot_ms = (now_100ns.wrapping_sub(ticks.wrapping_mul(10_000)) / 10_000) as i128;
    Some(format!("boot:{boot_ms}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_epoch() -> Option<String> {
    None
}

#[cfg(any(test, feature = "test-support"))]
type TestOverride = OnceLock<std::sync::Mutex<Option<(u32, Option<String>)>>>;

#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: TestOverride = OnceLock::new();

/// Test seam: force the epoch observed by `current()`. Only affects the
/// calling process, so cross-process contract tests still exercise the real
/// platform reading.
#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))] // seam referenced only from tests/binaries
pub(crate) fn set_epoch_for_tests(value: Option<String>) {
    let slot = TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some((std::process::id(), value));
}

#[cfg(any(test, feature = "test-support"))]
fn test_override() -> Option<Option<String>> {
    let slot = TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.as_ref() {
        // nextest runs tests in separate processes; the pid pin keeps an
        // override from leaking across a forked child in other harnesses.
        Some((pid, value)) if *pid == std::process::id() => Some(value.clone()),
        _ => None,
    }
}

#[cfg(not(any(test, feature = "test-support")))]
fn test_override() -> Option<Option<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_kind_values_match_within_tolerance_and_kinds_never_mix() {
        // Linux: boot UUID + start ticks must match exactly. Ticks alone are
        // NOT identity (every boot reads ~15 ticks — real 2026-09-27 reboot
        // finding on the personal Linux box).
        assert!(matches("pid1:uuid-a:15", "pid1:uuid-a:15"));
        assert!(!matches("pid1:uuid-a:15", "pid1:uuid-b:15"));
        assert!(!matches("pid1:uuid-a:15", "pid1:uuid-a:16"));
        assert!(!matches("pid1:uuid-a:15", "pid1:uuid-a:14"));
        // Boot wall time: nearby values are the same session (clock slew).
        assert!(matches("boot:1000", "boot:2900"));
        assert!(!matches("boot:1000", "boot:4000"));
        assert!(!matches("pid1:uuid-a:100", "boot:100"));
        assert!(!matches("garbage", "pid1:uuid-a:100"));
    }

    #[test]
    fn real_epoch_is_readable_and_stable_on_this_platform() {
        let first = current();
        let second = current();
        match (first, second) {
            (Some(a), Some(b)) => assert!(matches(&a, &b), "epoch must be stable: {a} vs {b}"),
            (None, None) => {}
            other => panic!("epoch reading must be deterministic: {other:?}"),
        }
    }
}

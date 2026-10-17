//! Unix execution observation is distinct from numeric PID-slot presence.
//! A zombie has exited and cannot hold a listening service or execute work;
//! an unreaped PID alone must not fence a successor. No signal is sent here.
use std::io;

use nix::sys::signal::kill;
use nix::unistd::Pid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Absent,
    Present,
    PermissionDenied,
}

impl Presence {
    fn is_present(self) -> bool {
        match self {
            Self::Absent => false,
            Self::Present | Self::PermissionDenied => true,
        }
    }
}

fn signal_presence(result: Result<(), nix::errno::Errno>) -> io::Result<Presence> {
    match result {
        Ok(()) => Ok(Presence::Present),
        Err(nix::errno::Errno::ESRCH) => Ok(Presence::Absent),
        // Darwin also returns EPERM for killpg(0) of an all-zombie group.
        // This is not exit proof or a live-member classification on its own.
        Err(nix::errno::Errno::EPERM) => Ok(Presence::PermissionDenied),
        Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

fn probe(pid: u32, include_root: bool) -> io::Result<(Presence, Presence)> {
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid execution-tree identifier",
            )
        })?;
    Ok((
        if include_root {
            signal_presence(kill(Pid::from_raw(pid), None))?
        } else {
            Presence::Absent
        },
        signal_presence(kill(Pid::from_raw(-pid), None))?,
    ))
}

/// True only when the target root or its process group has an executing member.
/// ESRCH proves absence; otherwise a complete PID/PGID/state snapshot separates
/// exited zombies from live/stopped/uninterruptible descendants. EPERM, failed
/// ps or a missing visible row for a still-present target remain errors.
/// The caller supplies its original absolute deadline around this future.
pub async fn has_live_members(pid: u32) -> io::Result<bool> {
    observe(pid, true).await
}

/// Group-only observation for a spawn-owned wrapper AFTER it independently
/// reaped its captured root. A reused unrelated numeric root PID is not part
/// of that original tree. This is insufficient proof for a raw legacy root.
pub async fn has_live_group_members(pid: u32) -> io::Result<bool> {
    observe(pid, false).await
}

async fn observe(pid: u32, include_root: bool) -> io::Result<bool> {
    loop {
        let before = probe(pid, include_root)?;
        if before == (Presence::Absent, Presence::Absent) {
            return Ok(false);
        }
        let output = tokio::process::Command::new("ps")
            .args(["-A", "-o", "pid=,pgid=,stat="])
            .kill_on_drop(true)
            .output()
            .await;
        let after = probe(pid, include_root)?;
        if after == (Presence::Absent, Presence::Absent) {
            return Ok(false);
        }
        // A root can exit between ps and signal(0); on Darwin this also turns
        // the group's response into zombie EPERM. Retry a complete observation
        // within the caller's same absolute deadline, never splice old live
        // state with a later permission response or invent a stable errno cause.
        if before != after {
            tokio::task::yield_now().await;
            continue;
        }
        let output = output?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "observe execution states: ps exited with {}",
                output.status
            )));
        }
        return classify_observation(pid, after.0, after.1, &output.stdout);
    }
}

fn classify_observation(
    pid: u32,
    leader: Presence,
    group: Presence,
    output: &[u8],
) -> io::Result<bool> {
    let live = classify_snapshot(pid, leader.is_present(), group.is_present(), output)?;
    if live && [leader, group].contains(&Presence::PermissionDenied) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "live execution target is inaccessible",
        ));
    }
    Ok(live)
}

fn classify_snapshot(
    pid: u32,
    leader_present: bool,
    group_present: bool,
    output: &[u8],
) -> io::Result<bool> {
    let text = std::str::from_utf8(output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut leader_seen = !leader_present;
    let mut group_seen = !group_present;
    let mut live = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let parse_id = |field: Option<&str>| {
            field
                .and_then(|field| field.parse::<u32>().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid PID/PGID process-state observation",
                    )
                })
        };
        let observed = parse_id(fields.next())?;
        let group = parse_id(fields.next())?;
        let state = fields.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process state is missing")
        })?;
        if fields.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected process-state columns",
            ));
        }
        let root_matches = leader_present && observed == pid;
        let group_matches = group_present && group == pid;
        if !root_matches && !group_matches {
            continue;
        }
        leader_seen |= root_matches;
        group_seen |= group_matches;
        live |= match state.as_bytes().first() {
            // Darwin ps(1): Z is dead/zombie. Linux also uses X/x for dead.
            Some(b'Z' | b'X' | b'x') => false,
            Some(b'I' | b'R' | b'S' | b'T' | b'U' | b'D' | b't' | b'W' | b'K' | b'P') => true,
            Some(_) | None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown target execution state",
                ));
            }
        };
    }
    if !leader_seen || !group_seen {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "present execution target is not visible in the process-state snapshot",
        ));
    }
    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::time::Duration;

    #[test]
    fn zombie_only_is_exited_but_live_descendant_or_unknown_observation_remains_protected() {
        assert!(!classify_snapshot(42, true, true, b"42 42 Z+\n43 42 Z\n").unwrap());
        assert!(!classify_snapshot(42, false, true, b"43 42 Z\n").unwrap());
        assert!(classify_snapshot(42, true, true, b"42 42 Z\n43 42 S\n").unwrap());
        assert!(classify_snapshot(42, true, true, b"42 42 ?\n").is_err());
        assert!(classify_snapshot(42, true, true, b"43 42 Z\n").is_err());
        assert!(classify_snapshot(42, true, true, b"42 99 Z\n").is_err());
        assert!(classify_snapshot(42, true, true, b"incomplete\n").is_err());
        // An unrelated process state cannot veto this target's observation.
        assert!(!classify_snapshot(42, true, true, b"42 42 Z\n999 999 ?\n").unwrap());
    }

    #[test]
    fn readable_zombie_group_is_quiescent_despite_darwin_eperm_but_live_or_hidden_group_is_protected()
     {
        assert!(
            !classify_observation(
                42,
                Presence::Present,
                Presence::PermissionDenied,
                b"42 42 Z\n43 42 Z\n"
            )
            .unwrap()
        );
        assert_eq!(
            classify_observation(
                42,
                Presence::Present,
                Presence::PermissionDenied,
                b"42 42 Z\n43 42 S\n"
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            classify_observation(42, Presence::Absent, Presence::PermissionDenied, b"").is_err()
        );
        // A process with this numeric PID in another group cannot fence an
        // original owned group whose root was independently reaped.
        assert!(
            !classify_observation(42, Presence::Absent, Presence::Absent, b"42 99 R\n").unwrap()
        );
    }

    #[test]
    fn permission_failure_is_not_exit_and_pid_existence_contract_is_unchanged() {
        assert_eq!(
            signal_presence(Err(nix::errno::Errno::ESRCH)).unwrap(),
            Presence::Absent
        );
        assert_eq!(signal_presence(Ok(())).unwrap(), Presence::Present);
        assert_eq!(
            signal_presence(Err(nix::errno::Errno::EPERM)).unwrap(),
            Presence::PermissionDenied
        );
        assert!(signal_presence(Err(nix::errno::Errno::EIO)).is_err());
    }

    #[tokio::test]
    async fn real_unreaped_zombie_is_quiescent_without_waiting_for_its_parent() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let quiescent = tokio::time::timeout(Duration::from_secs(3), async {
            while has_live_members(pid).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let slot_remains = crate::process_exists(pid).unwrap();
        let parent_reap = child.wait().unwrap();
        quiescent.unwrap();
        assert!(
            slot_remains,
            "the fixture must still be a real unreaped PID slot before parent wait"
        );
        assert!(parent_reap.success());
    }
}

#[cfg(test)]
mod failed_observation_tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    use std::time::Duration;

    #[tokio::test]
    async fn actual_ps_failure_does_not_turn_live_execution_into_absence() {
        const ENVIRONMENT: &str = "RCODER_TEST_PS_FAILURE_SUBPROCESS";
        if std::env::var_os(ENVIRONMENT).is_some() {
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap();
            let observed = has_live_members(child.id()).await;
            let still_running = child.try_wait().unwrap().is_none();
            child.kill().unwrap();
            let _exit = child.wait().unwrap();
            assert!(still_running);
            let error = observed.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert!(error.to_string().contains("ps exited"));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ps = directory.path().join("ps");
        std::fs::write(&ps, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(ps, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = tokio::time::timeout(Duration::from_secs(10), tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "execution_tree::failed_observation_tests::actual_ps_failure_does_not_turn_live_execution_into_absence", "--nocapture"])
            .env(ENVIRONMENT, "1").env("PATH", directory.path()).kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            output.status.success(),
            "actual observation failure fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

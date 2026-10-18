//! Resolve Windows Node tool shims without changing PATH/PATHEXT precedence.

use std::ffi::OsString;

/// Windows `Command` only appends `.exe` to a bare program. Use the existing
/// `which` resolver to include `.cmd`/`.bat` while preserving directory and
/// PATHEXT ordering. Explicit paths and missing programs keep their original
/// spawn behavior; other platforms pass the program through unchanged.
pub fn resolve_spawn_program(program: &str) -> OsString {
    #[cfg(windows)]
    if !program.is_empty()
        && std::path::Path::new(program).extension().is_none()
        && !program.contains(['/', '\\'])
        && let Ok(path) = which::which(program)
    {
        return path.into_os_string();
    }
    OsString::from(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_and_missing_programs_keep_their_spawn_behavior() {
        for program in [
            "",
            "./tools/build",
            ".\\tools\\build",
            "node.exe",
            "rcoder-fixture-no-such-program-532c",
        ] {
            assert_eq!(resolve_spawn_program(program), program);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_bare_program_is_not_resolved() {
        assert_eq!(resolve_spawn_program("npm"), "npm");
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_priority_and_shim_arguments_use_real_subprocesses() {
        const CASE: &str = "RCODER_TEST_SPAWN_RESOLVER_CASE";
        const VALUE: &str = "RCODER_TEST_SPAWN_RESOLVER_VALUE";
        const ARGUMENT: &str = "中文 argument with spaces & literal";
        const TEST: &str =
            "spawn_program::tests::windows_path_priority_and_shim_arguments_use_real_subprocesses";
        let case = std::env::var(CASE).ok();
        match case.as_deref() {
            Some("priority") => {
                assert_eq!(
                    resolve_spawn_program("native-fixture"),
                    std::env::var_os(VALUE).unwrap()
                );
                return;
            }
            Some("argument") => {
                assert_eq!(std::env::var(VALUE).unwrap(), ARGUMENT);
                return;
            }
            Some("shim") => {
                let observed = std::process::Command::new(resolve_spawn_program("native-fixture"))
                    .arg(ARGUMENT)
                    .env(
                        "RCODER_TEST_SPAWN_RESOLVER_EXECUTABLE",
                        std::env::current_exe().unwrap(),
                    )
                    .output()
                    .unwrap();
                assert!(
                    observed.status.success(),
                    "{} {}",
                    String::from_utf8_lossy(&observed.stdout),
                    String::from_utf8_lossy(&observed.stderr)
                );
                return;
            }
            Some(other) => panic!("unexpected subprocess case: {other}"),
            None => {}
        }

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("前位 path");
        let second = root.path().join("后位 path");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let executable = first.join("native-fixture.exe");
        std::fs::write(&executable, b"path selection only; never executed").unwrap();
        let shim = second.join("native-fixture.cmd");
        std::fs::write(&shim, b"@echo off\r\nset \"RCODER_TEST_SPAWN_RESOLVER_VALUE=%~1\"\r\nset \"RCODER_TEST_SPAWN_RESOLVER_CASE=argument\"\r\n\"%RCODER_TEST_SPAWN_RESOLVER_EXECUTABLE%\" --exact spawn_program::tests::windows_path_priority_and_shim_arguments_use_real_subprocesses --nocapture\r\n").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        let observed = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CASE, "priority")
            .env(VALUE, &executable)
            .env("PATH", path)
            .env("PATHEXT", ".EXE;.CMD;.BAT")
            .output()
            .unwrap();
        assert!(
            observed.status.success(),
            "{} {}",
            String::from_utf8_lossy(&observed.stdout),
            String::from_utf8_lossy(&observed.stderr)
        );

        // Run the actual shim from a path containing spaces and Unicode, and
        // let a fresh Rust process verify its argument without codepage loss.
        let observed = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CASE, "shim")
            .env("PATH", &second)
            .env("PATHEXT", ".EXE;.CMD;.BAT")
            .output()
            .unwrap();
        assert!(
            observed.status.success(),
            "{} {}",
            String::from_utf8_lossy(&observed.stdout),
            String::from_utf8_lossy(&observed.stderr)
        );
    }
}

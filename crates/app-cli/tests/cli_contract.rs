//! Exercise the shipped CLI without starting services; writes use temporary workspaces.
use std::process::{Command, Output};

fn command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("APP_CLI_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn cli(args: &[&str]) -> Output {
    command().args(args).output().expect("execute app-cli")
}

#[test]
fn explicit_commands_accept_their_options() {
    for args in [
        vec![
            "serve",
            "--workspace",
            "project with spaces",
            "--attach",
            "--help",
        ],
        vec!["run", "--workspace", "project with spaces", "--help"],
        vec![
            "build",
            "--workspace",
            "project with spaces",
            "--dev",
            "--help",
        ],
        vec!["gen-lock", "--workspace", "project with spaces", "--help"],
        vec![
            "run-service",
            "release1",
            "service1",
            "--log-dir",
            "logs",
            "--help",
        ],
    ] {
        let out = cli(&args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn old_top_level_forms_are_rejected() {
    for args in [
        vec!["--workspace", "project", "serve", "--help"],
        vec!["--gen-lock", "project", "--help"],
        vec!["--workspace", "project", "--help"],
        vec!["server", "--help"],
    ] {
        assert_eq!(cli(&args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn commands_reject_unrelated_options() {
    for args in [
        vec!["build", "--admin-addr", "127.0.0.1:3010", "--help"],
        vec!["build", "--attach", "--help"],
        vec!["run", "--attach", "--help"],
        vec!["gen-lock", "--dev", "--help"],
        vec!["serve", "--dev", "--help"],
    ] {
        assert_eq!(cli(&args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn command_is_required_and_help_is_scoped() {
    assert_eq!(cli(&[]).status.code(), Some(2));
    let out = cli(&["build", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).expect("UTF-8 help");
    assert!(help.contains("--workspace"));
    assert!(!help.contains("--admin-addr"));
    assert!(!help.contains("--attach"));
}

#[test]
fn gen_lock_dispatch_and_workspace_env_precedence() {
    let root = tempfile::tempdir().expect("temporary root");
    let env_workspace = root.path().join("env workspace");
    let explicit_workspace = root.path().join("explicit workspace");
    for workspace in [&env_workspace, &explicit_workspace] {
        std::fs::create_dir_all(workspace.join("web")).expect("create fixture");
        std::fs::write(
            workspace.join("workspace.manifest.toml"),
            include_str!("fixtures/minimal-workspace/workspace.manifest.toml"),
        )
        .expect("write workspace");
        std::fs::write(
            workspace.join("web/project.manifest.toml"),
            include_str!("fixtures/minimal-workspace/web/project.manifest.toml"),
        )
        .expect("write project");
    }
    let out = command()
        .args(["gen-lock", "--workspace"])
        .arg(&explicit_workspace)
        .env("APP_CLI_WORKSPACE", &env_workspace)
        // Build tools must dispatch before tracing initialization or runtime binding.
        .env("APP_CLI_LOG_DIR", root.path().join("unused logs"))
        .env("APP_CLI_ADMIN_ADDR", "not-an-address")
        .output()
        .expect("run gen-lock");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(explicit_workspace.join("release.lock.toml").is_file());
    assert!(!env_workspace.join("release.lock.toml").exists());
    assert!(!root.path().join("unused logs").exists());
    let out = command()
        .arg("gen-lock")
        .env("APP_CLI_WORKSPACE", &env_workspace)
        .output()
        .expect("run gen-lock with environment");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(env_workspace.join("release.lock.toml").is_file());
}

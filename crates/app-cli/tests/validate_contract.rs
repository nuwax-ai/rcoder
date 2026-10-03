//! Exercise configuration validation through the shipped binary, with no runtime environment.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const WORKSPACE: &str = "schema_version = 1\n[workspace]\nname = 'validate-fixture'\n";
const BUILD_COMMAND: &str = "command = [\"./missing-build.sh\", \"COMMAND_SECRET\"]";
const RUN_COMMAND: &str = "command = [\"./missing-start.sh\", \"ARGUMENT_SECRET\"]";

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command.env_clear();
    command
}

fn execute(workspace: &Path, subcommand: &str, options: &[&str]) -> Output {
    command()
        .arg(subcommand)
        .arg("--workspace")
        .arg(workspace)
        .args(options)
        .current_dir(workspace)
        .output()
        .expect("execute app-cli")
}

fn validate(workspace: &Path, dev: bool) -> Output {
    let options = if dev {
        vec!["--dev", "--json"]
    } else {
        vec!["--json"]
    };
    execute(workspace, "validate", &options)
}

fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // from_slice rejects extra JSON objects and tracing/preview text around the report.
    let report: Value = serde_json::from_slice(&output.stdout).expect("one JSON report");
    assert!(report.is_object());
    assert_eq!(report["report_version"], 1);
    assert_eq!(report["scope"], "configuration");
    assert_eq!(report["valid"], code == 0);
    assert!(report["services"].is_array());
    assert!(report["diagnostics"].is_array());
    assert!(report["not_checked"].is_array());
    assert!(report["skipped_checks"].is_array());
    report
}

fn fixture(root: &Path) -> PathBuf {
    let workspace = root.join("workspace with spaces");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::write(workspace.join("workspace.manifest.toml"), WORKSPACE)
        .expect("write workspace manifest");
    workspace
}

fn project(service_id: &str, depends_on: &[&str], proxy: &str) -> String {
    let dependencies = depends_on
        .iter()
        .map(|dependency| format!("'{dependency}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "schema_version = 1\n\
         [project]\nservice_id = '{service_id}'\nname = '{service_id}'\ntype = 'node'\n\
         [build]\n{BUILD_COMMAND}\nartifact = 'missing-dist'\n\
         [run]\n{RUN_COMMAND}\ndepends_on = [{dependencies}]\n\
         [env]\nPRIVATE_TOKEN = 'ENVIRONMENT_SECRET'\n\
         {proxy}"
    )
}

fn write_project(workspace: &Path, dir: &str, content: impl AsRef<[u8]>) -> PathBuf {
    let directory = workspace.join(dir);
    std::fs::create_dir_all(&directory).expect("create project directory");
    let path = directory.join("project.manifest.toml");
    std::fs::write(&path, content).expect("write project manifest");
    path
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(directory).expect("read fixture directory") {
            let entry = entry.expect("fixture entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("relative fixture path")
                .to_path_buf();
            if entry.file_type().expect("fixture file type").is_dir() {
                entries.insert(relative, None);
                visit(root, &path, entries);
            } else {
                entries.insert(
                    relative,
                    Some(std::fs::read(path).expect("read fixture file")),
                );
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn validate_help_and_options_are_scoped() {
    let output = command()
        .args(["validate", "--dev", "--json", "--help"])
        .output()
        .expect("validate help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    for text in [
        "--workspace",
        "--dev",
        "--json",
        "source configuration",
        "without writing files, building, or starting services",
        "configuration only",
    ] {
        assert!(help.contains(text), "missing {text}: {help}");
    }
    for option in ["--admin-addr", "--attach", "--deploy-dir", "--only"] {
        let output = command()
            .args(["validate", option, "unused", "--help"])
            .output()
            .expect("reject unrelated option");
        assert_eq!(output.status.code(), Some(2), "{option}");
    }
}

#[test]
fn workspace_selection_prefers_argument_then_environment_then_current_directory() {
    let root = tempfile::tempdir().unwrap();
    let mut locations = Vec::new();
    for name in ["cwd-web", "env-web", "argument-web"] {
        let base = root.path().join(name);
        std::fs::create_dir(&base).unwrap();
        let workspace = fixture(&base);
        write_project(&workspace, "web", project(name, &[], "[proxy]\npath='/'\n"));
        locations.push(workspace);
    }
    let before = snapshot(root.path());
    for (environment, argument, expected) in [
        (false, false, "cwd-web"),
        (true, false, "env-web"),
        (true, true, "argument-web"),
    ] {
        let mut invocation = command();
        invocation
            .args(["validate", "--json"])
            .current_dir(&locations[0]);
        if environment {
            invocation.env("APP_CLI_WORKSPACE", &locations[1]);
        }
        if argument {
            invocation.arg("--workspace").arg(&locations[2]);
        }
        let result = report(&invocation.output().unwrap(), 0);
        assert_eq!(result["services"][0]["service_id"], expected);
    }
    assert_eq!(
        snapshot(root.path()),
        before,
        "directory selection must not write state"
    );
}

#[test]
fn successful_validate_checks_only_configuration_and_preserves_every_file() {
    let root = tempfile::tempdir().expect("temporary fixture");
    let workspace = fixture(root.path());
    write_project(
        &workspace,
        "web",
        project("web", &[], "[proxy]\npath = '/'\n"),
    );
    std::fs::write(
        workspace.join("release.lock.toml"),
        "EXISTING_LOCK_SENTINEL",
    )
    .expect("write old lock");
    let before = snapshot(root.path());
    let output = command()
        .args(["validate", "--json", "--workspace"])
        .arg(&workspace)
        .env("APP_CLI_LOG_DIR", root.path().join("unused logs"))
        .env("APP_CLI_STATE_ROOT", root.path().join("unused state"))
        .env(
            "APP_CLI_PINGAP_RUNTIME_DIR",
            root.path().join("unused proxy"),
        )
        .env("APP_CLI_PINGAP_BIN", root.path().join("missing pingap"))
        .env("APP_CLI_ADMIN_ADDR", "not-an-address")
        .env("APP_DEPLOY_URL", "https://unused.invalid/DEPLOY_SECRET")
        .env("APP_CLI_WORKSPACE", root.path().join("wrong workspace"))
        .output()
        .expect("validate without runtime");
    let report = report(&output, 0);
    assert_eq!(report["profile"], "prod");
    assert_eq!(report["topology_checked"], true);
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .is_empty()
    );
    assert!(
        !report["not_checked"]
            .as_array()
            .expect("not checked")
            .is_empty()
    );
    assert_eq!(report["services"][0]["service_id"], "web");
    assert_eq!(report["services"][0]["proxy_path"], "/");
    assert!(report["services"][0]["port"].as_u64().expect("port") > 0);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 JSON");
    for secret in [
        "ENVIRONMENT_SECRET",
        "ARGUMENT_SECRET",
        "COMMAND_SECRET",
        "DEPLOY_SECRET",
    ] {
        assert!(!stdout.contains(secret), "report leaked {secret}");
    }
    assert!(!workspace.join("web/missing-dist").exists());
    assert!(!workspace.join("web/missing-start.sh").exists());
    assert_eq!(
        snapshot(root.path()),
        before,
        "validate must not create logs, owner state, lock, or proxy files"
    );
}

#[test]
fn observation_failure_has_priority_and_suppresses_incomplete_topology_checks() {
    let root = tempfile::tempdir().expect("temporary fixture");
    let workspace = fixture(root.path());
    write_project(&workspace, "a", b"\xff\xfe\xfd");
    write_project(
        &workspace,
        "b",
        project("b", &[], "").replace(BUILD_COMMAND, "command = []"),
    );
    write_project(
        &workspace,
        "c",
        project("c", &["a", "b"], "[proxy]\npath = '/'\n"),
    );
    let before = snapshot(root.path());
    let report = report(&validate(&workspace, false), 3);
    assert_eq!(report["topology_checked"], false);
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|issue| { issue["kind"] == "io" && issue["file"] == "a/project.manifest.toml" })
    );
    assert!(diagnostics.iter().any(|issue| {
        issue["kind"] == "validation"
            && issue["file"] == "b/project.manifest.toml"
            && issue["field"] == "build.command"
    }));
    assert!(
        !diagnostics.iter().any(|issue| {
            issue["field"] == "run.depends_on"
                || issue["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("missing or disabled"))
        }),
        "an unreadable project must not fabricate missing dependencies: {report}"
    );
    assert!(
        !report["skipped_checks"]
            .as_array()
            .expect("skipped checks")
            .is_empty()
    );
    assert_eq!(snapshot(root.path()), before);
}

#[test]
fn parse_failures_locate_files_without_echoing_source_or_secrets() {
    let root = tempfile::tempdir().expect("temporary fixture");
    let workspace = fixture(root.path());
    let unknown = project("unknown", &[], "[proxy]\npath = '/'\n").replace(
        "type = 'node'",
        "type = 'node'\nunknown_field = 'UNKNOWN_VALUE_SECRET'",
    );
    write_project(&workspace, "unknown", unknown);
    write_project(
        &workspace,
        "syntax",
        "schema_version = 1\n[project]\nname = 'SYNTAX_VALUE_SECRET\n",
    );
    let output = validate(&workspace, false);
    let report = report(&output, 1);
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics");
    for file in [
        "unknown/project.manifest.toml",
        "syntax/project.manifest.toml",
    ] {
        let diagnostic = diagnostics
            .iter()
            .find(|issue| issue["file"] == file)
            .expect("located diagnostic");
        assert_eq!(diagnostic["kind"], "parse");
        let location = format!("{} {}", diagnostic["message"], diagnostic["hint"]);
        assert!(
            location.contains("line") && location.contains("column"),
            "missing parse position: {diagnostic}"
        );
    }
    let output_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [
        "UNKNOWN_VALUE_SECRET",
        "SYNTAX_VALUE_SECRET",
        "ENVIRONMENT_SECRET",
        "ARGUMENT_SECRET",
    ] {
        assert!(!output_text.contains(secret), "diagnostics leaked {secret}");
    }
}

#[test]
fn workspace_and_custom_proxy_errors_distinguish_observation_from_validation() {
    for (workspace_manifest, proxy_file, expected_code, expected_kind, expected_file) in [
        (None, None, 3, "io", "workspace.manifest.toml"),
        (
            Some("schema_version = 1\n[workspace]\nname = ''\n"),
            None,
            1,
            "validation",
            "workspace.manifest.toml",
        ),
        (
            Some(
                "schema_version = 1\n[workspace]\nname = 'custom'\n[pingap]\nmode = 'custom'\nconfig = 'pingap.toml'\n",
            ),
            None,
            3,
            "io",
            "pingap.toml",
        ),
        (
            Some(
                "schema_version = 1\n[workspace]\nname = 'custom'\n[pingap]\nmode = 'custom'\nconfig = 'pingap.toml'\n",
            ),
            Some("[servers.main]\naddr = '0.0.0.0:9999'\n"),
            1,
            "validation",
            "pingap.toml",
        ),
    ] {
        let root = tempfile::tempdir().expect("temporary fixture");
        let workspace = fixture(root.path());
        write_project(
            &workspace,
            "web",
            project("web", &[], "[proxy]\npath = '/'\n"),
        );
        if let Some(manifest) = workspace_manifest {
            std::fs::write(workspace.join("workspace.manifest.toml"), manifest)
                .expect("workspace manifest");
        } else {
            std::fs::remove_file(workspace.join("workspace.manifest.toml"))
                .expect("remove manifest");
        }
        if let Some(config) = proxy_file {
            std::fs::write(workspace.join("pingap.toml"), config).expect("custom config");
        }
        let before = snapshot(root.path());
        let report = report(&validate(&workspace, false), expected_code);
        assert!(
            report["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|issue| { issue["kind"] == expected_kind && issue["file"] == expected_file }),
            "missing {expected_kind} for {expected_file}: {report}"
        );
        assert_eq!(snapshot(root.path()), before);
    }
}

#[test]
fn dev_profile_changes_effective_route_and_keeps_production_requirements() {
    let root = tempfile::tempdir().expect("temporary fixture");
    let workspace = fixture(root.path());
    let content = project(
        "web",
        &[],
        "[devrun]\ncommand = ['./missing-dev-server']\n\
         [devbuild]\ncommand = ['./missing-dev-build']\n\
         [proxy]\npath = '/web/'\nstrip_prefix = true\ndev_strip_prefix = false\n",
    );
    let manifest = write_project(&workspace, "web", &content);
    for (dev, profile, strip_prefix) in [(false, "prod", true), (true, "dev", false)] {
        let report = report(&validate(&workspace, dev), 0);
        assert_eq!(report["profile"], profile);
        assert_eq!(report["services"][0]["proxy_path"], "/web/");
        assert_eq!(
            report["services"][0]["effective_strip_prefix"],
            strip_prefix
        );
    }
    for (command, field) in [
        (BUILD_COMMAND, "build.command"),
        (RUN_COMMAND, "run.command"),
    ] {
        std::fs::write(&manifest, content.replace(command, "command = []"))
            .expect("invalid production command");
        for dev in [false, true] {
            let report = report(&validate(&workspace, dev), 1);
            assert!(
                report["diagnostics"]
                    .as_array()
                    .expect("diagnostics")
                    .iter()
                    .any(|issue| { issue["kind"] == "validation" && issue["field"] == field }),
                "--dev must retain {field}: {report}"
            );
        }
    }
}

#[test]
fn validate_and_gen_lock_share_ports_topology_and_preserve_old_lock_on_failure() {
    let root = tempfile::tempdir().expect("temporary fixture");
    let workspace = fixture(root.path());
    // Lexical order differs from dependency order, so this verifies real topology sorting.
    let api = write_project(
        &workspace,
        "a-api",
        project("api", &["db"], "[proxy]\npath = '/'\n"),
    );
    write_project(&workspace, "z-db", project("db", &[], ""));
    let lock_path = workspace.join("release.lock.toml");
    std::fs::write(&lock_path, "OLD_LOCK_SENTINEL").expect("old lock");
    let validation = report(&validate(&workspace, false), 0);
    assert_eq!(
        std::fs::read(&lock_path).expect("preserved old lock"),
        b"OLD_LOCK_SENTINEL"
    );
    let output = execute(&workspace, "gen-lock", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock_bytes = std::fs::read(&lock_path).expect("generated lock");
    let lock = workspace_manifest::load_release_lock(
        std::str::from_utf8(&lock_bytes).expect("UTF-8 lock"),
    )
    .expect("load generated lock");
    assert_eq!(
        lock.services
            .iter()
            .map(|service| service.service_id.as_str())
            .collect::<Vec<_>>(),
        ["db", "api"]
    );
    let services = validation["services"].as_array().expect("preview services");
    assert_eq!(services.len(), lock.services.len());
    for (preview, locked) in services.iter().zip(&lock.services) {
        assert_eq!(preview["service_id"], locked.service_id);
        assert_eq!(preview["dir"], locked.dir);
        assert_eq!(preview["port"], locked.port);
    }
    let invalid =
        project("api", &["db"], "[proxy]\npath = '/'\n").replace(BUILD_COMMAND, "command = []");
    std::fs::write(api, invalid).expect("write invalid manifest");
    report(&validate(&workspace, false), 1);
    let output = execute(&workspace, "gen-lock", &[]);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(&lock_path).expect("lock after failure"),
        lock_bytes
    );
}

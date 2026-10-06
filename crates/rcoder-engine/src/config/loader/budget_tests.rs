//! Child processes exercise the production config loader without global env mutation.
use super::{create_default_config_file, load_config_with_args};
use crate::config::CliArgs;
use clap::Parser;
use std::ffi::OsString;
use std::process::Command;

const SQL_KEY: &str = "RCODER_USERAPP_DEPLOY_SQL_STAGE_BUDGET_SECS";
const THRESHOLD_KEY: &str = "RCODER_USERAPP_DEPLOY_FAILURE_RESTART_THRESHOLD";
const PROBE_MODE: &str = "RCODER_TEST_BUDGET_PROBE_MODE";
const PROBE_KEY: &str = "RCODER_TEST_BUDGET_PROBE_KEY";

#[test]
fn explicit_restart_admission_wait_configuration_is_validated() {
    let fixture = |enabled: bool, seconds: &str| {
        let mut document = serde_yaml::to_value(crate::config::AppConfig::default()).unwrap();
        document["app_manager"]["enabled"] = serde_yaml::Value::Bool(enabled);
        document["app_manager"]["restart_admission_wait_secs"] =
            serde_yaml::from_str(seconds).unwrap();
        serde_yaml::to_string(&document).unwrap()
    };
    let selected = super::parse_config_with_deploy_budget(&fixture(true, "7")).unwrap();
    assert_eq!(selected.app_manager.restart_admission_wait_secs, 7);
    for value in ["0", "18446744073709551615"] {
        let error = super::parse_config_with_deploy_budget(&fixture(true, value)).unwrap_err();
        assert!(error.downcast_ref::<shared_types::AppError>().is_some());
    }
    assert!(
        !super::parse_config_with_deploy_budget(&fixture(false, "0"))
            .unwrap()
            .app_manager
            .enabled
    );
}

fn child(mode: &str, key: &str, value: OsString, explicit: bool) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yml");
    let mut fixture = crate::config::AppConfig {
        proxy_config: None,
        docker_config: None,
        ..crate::config::AppConfig::default()
    };
    fixture.api_key_auth.api_key = "controlled-fixture-key".into();
    fixture.app_manager.enabled = mode != "disabled";
    let mut document = serde_yaml::to_value(fixture).unwrap();
    let app_manager = document["app_manager"].as_mapping_mut().unwrap();
    let budget_key = serde_yaml::Value::String("deploy_budget".into());
    if explicit {
        app_manager.insert(
            budget_key,
            serde_yaml::from_str("sql_stage_budget_secs: 42").unwrap(),
        );
    } else {
        app_manager.remove(&budget_key);
    }
    if mode != "generated" {
        std::fs::write(&path, serde_yaml::to_string(&document).unwrap()).unwrap();
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    for key in [
        SQL_KEY,
        THRESHOLD_KEY,
        "RCODER_USERAPP_DEPLOY_NO_PROGRESS_TIMEOUT_SECS",
        "RCODER_USERAPP_DEPLOY_PRE_APPCLI_STAGE_BUDGET_SECS",
        "RCODER_USERAPP_DEPLOY_ABSOLUTE_BUDGET_SECS",
        "RCODER_USERAPP_DEPLOY_OOM_RESTART_THRESHOLD",
        "RCODER_USERAPP_DEPLOY_FENCED_ALERT_AFTER_SECS",
    ] {
        command.env_remove(key);
    }
    command
        .args([
            "--exact",
            "config::loader::budget_tests::config_budget_loader_child_probe",
            "--nocapture",
        ])
        .env("RCODER_CONFIG_FILE", path)
        .env_remove("RCODER_DEPLOY_HOST_REACH")
        .env(PROBE_MODE, mode)
        .env(PROBE_KEY, key)
        .env(key, value)
        .output()
        .unwrap()
}

#[test]
fn config_budget_loader_child_probe() {
    let Ok(mode) = std::env::var(PROBE_MODE) else {
        // Running this helper directly still asserts a real default contract.
        let defaults = crate::config::AppConfig::default();
        assert_eq!(
            defaults.app_manager.deploy_budget.absolute_budget_secs,
            3600
        );
        return;
    };
    if mode == "generated" {
        // Exercise the real generator before adapting only unrelated Docker /
        // proxy configuration in this isolated file. The in-memory Docker
        // defaults require runner images and cannot validate without them.
        create_default_config_file().unwrap();
        let path = std::env::var_os("RCODER_CONFIG_FILE").unwrap();
        let mut generated: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            generated
                .get("app_manager")
                .and_then(|section| section.get("deploy_budget"))
                .is_none(),
            "real default generation must not persist an explicit budget section"
        );
        generated["docker_config"] = serde_yaml::Value::Null;
        generated["proxy_config"] = serde_yaml::Value::Null;
        std::fs::write(&path, serde_yaml::to_string(&generated).unwrap()).unwrap();
    }
    let loaded = std::panic::catch_unwind(|| {
        load_config_with_args(CliArgs::try_parse_from(["rcoder"]).unwrap())
    })
    .expect("invalid selected environment input must return an error, never panic");
    if mode == "invalid" {
        let error = loaded.expect_err("invalid selected budget input must fail");
        let key = std::env::var(PROBE_KEY).unwrap();
        assert!(
            error.to_string().contains(&key),
            "error must identify the invalid field: {error:#}"
        );
        let structured = error
            .downcast_ref::<shared_types::AppError>()
            .expect("configuration origin must remain typed");
        match structured {
            shared_types::AppError::Structured(detail) => {
                assert_eq!(detail.code, "ERR_RUNTIME_CONFIGURATION")
            }
            _ => panic!("configuration failure must have a specific code"),
        }
    } else {
        let config = loaded.expect("corrected or explicit configuration must load");
        let expected = if mode == "explicit" {
            42
        } else if mode == "disabled" {
            1800
        } else {
            27
        };
        assert_eq!(
            config.app_manager.deploy_budget.sql_stage_budget_secs,
            expected
        );
        if mode == "generated" {
            let path = std::env::var_os("RCODER_CONFIG_FILE").unwrap();
            let generated: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(
                generated
                    .get("app_manager")
                    .and_then(|section| section.get("deploy_budget"))
                    .is_none(),
                "default generation must not freeze environment input as an explicit budget section"
            );
            let reloaded =
                load_config_with_args(CliArgs::try_parse_from(["rcoder"]).unwrap()).unwrap();
            assert_eq!(
                reloaded.app_manager.deploy_budget.sql_stage_budget_secs, 27,
                "generated file must keep selecting current environment on the next load"
            );
        }
        if mode == "disabled" {
            assert!(
                !config.app_manager.enabled,
                "unrelated budget input must not enable a disabled module"
            );
        }
        if mode == "explicit" {
            assert_eq!(
                config.app_manager.deploy_budget.failure_restart_threshold, 3,
                "explicit budget section must keep its own defaults rather than merge environment"
            );
        }
    }
}

#[test]
fn config_budget_invalid_u64_returns_structured_error() {
    let output = child("invalid", SQL_KEY, "not-a-number".into(), false);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_budget_invalid_u32_returns_structured_error() {
    let output = child("invalid", THRESHOLD_KEY, "not-a-number".into(), false);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_budget_explicit_section_precedes_environment_defaults() {
    let output = child("explicit", SQL_KEY, "not-a-number".into(), true);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_budget_corrected_input_reloads_successfully() {
    let bad = child("invalid", SQL_KEY, "not-a-number".into(), false);
    let corrected = child("valid", SQL_KEY, "27".into(), false);
    assert!(
        corrected.status.success(),
        "{}{}",
        String::from_utf8_lossy(&corrected.stdout),
        String::from_utf8_lossy(&corrected.stderr)
    );
    assert!(
        bad.status.success(),
        "{}{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
}

#[cfg(unix)]
#[test]
fn config_budget_non_unicode_selected_input_is_not_silently_ignored() {
    use std::os::unix::ffi::OsStringExt;
    let output = child("invalid", SQL_KEY, OsString::from_vec(vec![0xff]), false);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_budget_disabled_module_does_not_parse_unrelated_environment() {
    let output = child("disabled", SQL_KEY, "not-a-number".into(), false);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_budget_generated_default_keeps_environment_selection_on_reload() {
    let output = child("generated", SQL_KEY, "27".into(), false);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

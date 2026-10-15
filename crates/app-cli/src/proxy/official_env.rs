//! Foreground ownership must not be changed by inherited Pingap CLI options.
use std::collections::BTreeMap;
use tokio::process::Command;

const CONTROL_ENVIRONMENT: &[&str] = &[
    "PINGAP_DAEMON",
    "PINGAP_UPGRADE",
    "PINGAP_CP",
    "PINGAP_AUTORELOAD",
    "PINGAP_AUTORESTART",
    "PINGAP_READY_SOCK",
    "PINGAP_CONF",
    "PINGAP_ADMIN_ADDR",
    "PINGAP_ADMIN_USER",
    "PINGAP_ADMIN_PASSWORD",
    "PINGAP_LOG",
];

pub(crate) fn sanitize(command: &mut Command) {
    for key in CONTROL_ENVIRONMENT {
        command.env_remove(key);
    }
}

/// Supervisord inherits its own environment. Empty values disable the pinned
/// upstream CLI environment fallbacks; the host overlays its chosen admin
/// address and credentials afterwards.
pub(crate) fn cleared_environment() -> BTreeMap<String, String> {
    CONTROL_ENVIRONMENT
        .iter()
        .map(|key| (String::from(*key), String::new()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_options_are_removed_without_erasing_candidate_environment() {
        let mut command = Command::new("pingap");
        command.env("BUSINESS_RESOURCE", "/workspace/assets");
        command.env("PINGAP_DAEMON", "1");
        sanitize(&mut command);
        let overrides: BTreeMap<_, _> = command.as_std().get_envs().collect();
        for key in CONTROL_ENVIRONMENT {
            assert_eq!(overrides.get(std::ffi::OsStr::new(key)), Some(&None));
        }
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("BUSINESS_RESOURCE")),
            Some(&Some(std::ffi::OsStr::new("/workspace/assets")))
        );
        assert!(cleared_environment().values().all(String::is_empty));
    }
}

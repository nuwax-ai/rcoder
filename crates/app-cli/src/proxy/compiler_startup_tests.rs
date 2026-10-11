//! Selection tests complement the real owner/official Pingap fixture. They do
//! not claim a native proxy acceptance from an in-memory configuration.
use super::{StartupContext, StartupHost, business_http_targets};
use workspace_manifest::{ProjectKind, ProjectType, StartupProbe};

fn release() -> workspace_manifest::ReleaseLock {
    workspace_manifest::load_release_lock(include_str!(
        "../../../workspace-manifest/tests/fixtures/lock_v1.toml"
    ))
    .unwrap()
}

#[test]
fn business_http_confirmation_matches_effective_startup_strategy() {
    for host in [StartupHost::Builtin, StartupHost::Supervisord] {
        for dev_profile in [false, true] {
            for using_devrun in [false, true] {
                for probe in [None, Some(StartupProbe::Http), Some(StartupProbe::Tcp)] {
                    let mut release = release();
                    release.services.truncate(1);
                    let service = &mut release.services[0];
                    service.enabled = true;
                    service.kind = ProjectKind::Web;
                    service.r#type = ProjectType::Python;
                    service.devrun = using_devrun.then(|| workspace_manifest::DevrunSection {
                        command: vec!["development-server".into()],
                    });
                    service.health.startup_probe = probe;
                    let expected = match probe {
                        Some(StartupProbe::Http) => true,
                        Some(StartupProbe::Tcp) => false,
                        Some(StartupProbe::Process) => unreachable!("Web process probe is invalid"),
                        None => match host {
                            StartupHost::Builtin => !(dev_profile && using_devrun),
                            StartupHost::Supervisord => false,
                        },
                    };
                    let context = StartupContext { dev_profile, host };
                    let expected_service = service.service_id.clone();
                    let expected_path = service.health.readiness_path.clone();
                    let probes = business_http_targets(&release, context);
                    assert_eq!(
                        probes.len(),
                        usize::from(expected),
                        "host={host:?}, dev={dev_profile}, devrun={using_devrun}, probe={probe:?}"
                    );
                    if expected {
                        assert_eq!(probes[0].service_id, expected_service);
                        assert_eq!(probes[0].path, expected_path);
                    }
                }
            }
        }
    }
}

#[test]
fn static_http_is_preserved_and_worker_disabled_or_unproxied_are_not_added() {
    let mut release = release();
    release.services.truncate(1);
    release.services[0].r#type = ProjectType::Static;
    release.services[0].health.startup_probe = None;
    release.services[0].devrun = None;
    for context in [
        StartupContext::builtin(false),
        StartupContext::supervisord(true),
    ] {
        assert_eq!(
            business_http_targets(&release, context).len(),
            1,
            "platform static hosting retains HTTP confirmation on both hosts"
        );
    }
    release.services[0].enabled = false;
    assert!(business_http_targets(&release, StartupContext::builtin(false)).is_empty());
    release.services[0].enabled = true;
    release.services[0].kind = ProjectKind::Worker;
    release.services[0].health.startup_probe = Some(StartupProbe::Process);
    assert!(business_http_targets(&release, StartupContext::builtin(false)).is_empty());
    release.services[0].kind = ProjectKind::Web;
    release.services[0].health.startup_probe = Some(StartupProbe::Http);
    release.services[0].proxy = None;
    assert!(business_http_targets(&release, StartupContext::supervisord(false)).is_empty());
}

#[test]
fn supervisord_bridge_remains_independent_of_the_proxy_publication_gate() {
    let mut release = release();
    release.services.truncate(1);
    release.bridge_service = Some(release.services[0].service_id.clone());
    release.services[0].health.startup_probe = None;
    assert!(
        business_http_targets(&release, StartupContext::supervisord(false)).is_empty(),
        "legacy startsecs plus soft bridge readiness must not become mandatory HTTP publication"
    );
    release.services[0].health.startup_probe = Some(StartupProbe::Http);
    assert_eq!(
        business_http_targets(&release, StartupContext::supervisord(true)).len(),
        1,
        "an explicit HTTP strategy remains mandatory even for a bridge service"
    );
}

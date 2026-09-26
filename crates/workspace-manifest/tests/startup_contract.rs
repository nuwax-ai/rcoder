use workspace_manifest::*;

fn mixed() -> ReleaseLock {
    let mut lock = load_release_lock(include_str!("fixtures/lock_v1.toml")).unwrap();
    let mut worker = lock.services[0].clone();
    worker.service_id = "worker".into();
    worker.dir = "worker".into();
    worker.kind = ProjectKind::Worker;
    worker.proxy = None;
    worker.port += 1;
    worker.health = HealthSection {
        startup_probe: Some(StartupProbe::Process),
        ..Default::default()
    };
    lock.services.push(worker);
    lock
}

#[test]
fn wire_contract_preserves_legacy_and_explicit_strategies() {
    let legacy = load_release_lock(include_str!("fixtures/lock_v1.toml")).unwrap();
    let wire = toml::to_string(&legacy).unwrap();
    assert!(!wire.contains("startup_probe"));
    require_startup_probe_capability(&legacy, &[]).unwrap();
    // An old strict reader accepts the absent field and rejects any explicit strategy.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldHealth {
        startup_path: String,
        readiness_path: String,
        liveness_path: String,
    }
    let old: OldHealth =
        toml::from_str(&toml::to_string(&HealthSection::default()).unwrap()).unwrap();
    assert_eq!(
        (
            old.startup_path.as_str(),
            old.readiness_path.as_str(),
            old.liveness_path.as_str()
        ),
        ("/health", "/health", "/health")
    );
    for probe in [StartupProbe::Http, StartupProbe::Tcp, StartupProbe::Process] {
        let mut lock = mixed();
        lock.services[1].health.startup_probe = Some(probe);
        let wire = toml::to_string(&lock).unwrap();
        let read = load_release_lock(&wire).unwrap();
        assert_eq!(read.services[1].health.startup_probe, Some(probe));
        assert!(require_startup_probe_capability(&read, &[]).is_err());
        require_startup_probe_capability(&read, &[STARTUP_PROBE_CAPABILITY.into()]).unwrap();
        assert!(
            toml::from_str::<OldHealth>(&toml::to_string(&read.services[1].health).unwrap())
                .is_err()
        );
    }
}

#[test]
fn direct_lock_rejects_invalid_startup_combinations() {
    let cases: &[fn(&mut ReleaseLock)] = &[
        |l| l.services[1].kind = ProjectKind::Web,
        |l| l.services[1].r#type = ProjectType::Static,
        |l| l.services[1].proxy = l.services[0].proxy.clone(),
        |l| l.bridge_service = Some("worker".into()),
        |l| l.services[1].health.readiness_path = "/ready".into(),
        |l| l.services[1].health.startup_timeout_seconds = 5,
        |l| l.services[1].run.command.clear(),
        |l| l.services[1].devrun = Some(DevrunSection { command: vec![] }),
        |l| {
            l.services.remove(0);
            l.bridge_service = None;
        },
    ];
    for (index, change) in cases.iter().enumerate() {
        let mut lock = mixed();
        change(&mut lock);
        assert!(
            load_release_lock(&toml::to_string(&lock).unwrap()).is_err(),
            "case {index}"
        );
    }
}

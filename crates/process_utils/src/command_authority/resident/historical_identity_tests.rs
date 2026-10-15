use super::*;

fn fixture() -> (tempfile::TempDir, PathBuf, ResidentIdentity) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&root).unwrap();
    let identity = ResidentIdentity {
        application_id: "previous-named-app".into(),
        binding: serde_json::json!({"component":"app-cli","resource":"/home/user/81/code"}),
        owner_instance: root.file_name().unwrap().to_str().unwrap().into(),
        physical_domain: Some(
            serde_json::json!({"authority":"owned-platform","volume":"retained-pvc","instance":"old-pod"}),
        ),
        process_epoch: Some("original-process-space".into()),
    };
    (temporary, root, identity)
}

#[test]
fn historical_header_diagnostics_preserve_identity_and_strict_current_validation() {
    for scenario in ["version", "phase", "extra", "missing-diagnostics"] {
        let (_temporary, root, identity) = fixture();
        let mut value =
            serde_json::json!({"version":1,"identity":identity,"phase":"Running","error":null});
        match scenario {
            "version" => value["version"] = 999.into(),
            "phase" => value["phase"] = "FutureHistorical".into(),
            "extra" => value["historical_diagnostic"] = serde_json::json!({"keep":[1,2,3]}),
            _ => {
                value.as_object_mut().unwrap().remove("version");
                value.as_object_mut().unwrap().remove("phase");
            }
        }
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        let path = root.join("resident-scope.json");
        std::fs::write(&path, &bytes).unwrap();
        // This is the old admission counterexample, not a changed current
        // spawn permission: strict authorization still rejects the same bytes.
        assert!(read_identity(&root).is_err(), "{scenario}");
        assert!(validate_resident_spawn(&root).is_err(), "{scenario}");
        for _ in 0..2 {
            assert_eq!(read_historical_identity(&root).unwrap(), identity);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}

#[test]
fn historical_identity_does_not_guess_unreadable_or_changed_owner_binding() {
    for scenario in [
        "missing-identity",
        "missing-binding",
        "owner",
        "invalid-owner-type",
        "malformed",
    ] {
        let (_temporary, root, identity) = fixture();
        let mut value =
            serde_json::json!({"version":999,"identity":identity,"phase":"FutureHistorical"});
        match scenario {
            "missing-identity" => {
                value.as_object_mut().unwrap().remove("identity");
            }
            "missing-binding" => {
                value["identity"].as_object_mut().unwrap().remove("binding");
            }
            "owner" => {
                value["identity"]["owner_instance"] = uuid::Uuid::new_v4().to_string().into()
            }
            "invalid-owner-type" => value["identity"]["owner_instance"] = 17.into(),
            _ => {}
        }
        let bytes = if scenario == "malformed" {
            b"{broken historical scope".to_vec()
        } else {
            serde_json::to_vec(&value).unwrap()
        };
        let path = root.join("resident-scope.json");
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_historical_identity(&root).is_err(), "{scenario}");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

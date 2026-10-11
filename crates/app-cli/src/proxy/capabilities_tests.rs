use super::*;

#[test]
fn binary_digest_future_does_not_embed_large_io_buffers() {
    let future = binary_digest(Path::new("not-polled-official-proxy"));
    let bytes = std::mem::size_of_val(&future);
    assert!(
        bytes <= 8192,
        "binary digest future embeds {bytes} bytes; nested startup futures must keep I/O buffers on the heap"
    );
}

#[tokio::test]
async fn cached_capability_digest_cannot_validate_an_atomically_replaced_executable() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("official-proxy");
    tokio::fs::write(&binary, b"first immutable official executable")
        .await
        .unwrap();
    let digest = binary_digest(&binary).await.unwrap();
    cache()
        .lock()
        .unwrap()
        .entry(digest.clone())
        .or_insert_with(|| Arc::new(BTreeSet::from(["directory".into()])));
    let deadline = Instant::now() + Duration::from_secs(2);
    let observed = preflight_until(&binary, &PingapConfig::default(), deadline)
        .await
        .unwrap();
    assert_eq!(observed, digest);
    let replacement = root.path().join("replacement");
    tokio::fs::write(
        &replacement,
        b"different executable with a different plugin set",
    )
    .await
    .unwrap();
    let replacement_digest = binary_digest(&replacement).await.unwrap();
    std::fs::rename(&replacement, &binary).unwrap();
    let error = verify_binary_until(&binary, &observed, deadline)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("changed between capability preflight"));
    assert!(
        !cache().lock().unwrap().contains_key(&replacement_digest),
        "a post-test mutation must not manufacture validated replacement capabilities"
    );
}

fn basic(pid: u32) -> BasicCapabilities {
    BasicCapabilities {
        pid: pid.to_string(),
        version: PINNED_VERSION.into(),
        git_hash: PINNED_COMMIT.into(),
        supported_plugins: vec!["mock".into(), "directory".into()],
    }
}

#[test]
fn capabilities_require_the_owned_worker_and_exact_official_source() {
    let mut response = basic(19);
    assert!(validate_basic(&response, 19).is_ok());
    assert!(
        validate_basic(&response, 18)
            .unwrap_err()
            .to_string()
            .contains("another process")
    );
    response.git_hash = "unrelated-source".into();
    assert!(validate_basic(&response, 19).is_err());
    response.git_hash = PINNED_COMMIT.into();
    response.supported_plugins.clear();
    assert!(validate_basic(&response, 19).is_err());
}

#[test]
fn unavailable_plugin_and_missing_reference_are_rejected_before_native_test() {
    let mut config = PingapConfig::default();
    let supported = BTreeSet::from(["mock".into()]);
    config.plugins.insert(
        "candidate".into(),
        toml::from_str("category = 'not_in_this_binary'").unwrap(),
    );
    assert!(
        validate_plugins(&config, &supported)
            .unwrap_err()
            .to_string()
            .contains("unavailable official Pingap category")
    );
    config.plugins.insert(
        "candidate".into(),
        toml::from_str("category = 'mock'").unwrap(),
    );
    config.locations.insert(
        "route".into(),
        pingap_config::LocationConf {
            plugins: Some(vec!["missing".into()]),
            ..Default::default()
        },
    );
    assert!(
        validate_plugins(&config, &supported)
            .unwrap_err()
            .to_string()
            .contains("references missing plugin")
    );
    config.locations.get_mut("route").unwrap().plugins = Some(vec!["candidate".into()]);
    assert!(validate_plugins(&config, &supported).is_ok());
}

#[test]
fn pinned_builtin_aliases_require_their_actual_category() {
    let mut config = PingapConfig::default();
    config.locations.insert(
        "route".into(),
        pingap_config::LocationConf {
            plugins: Some(vec![
                "pingap:requestId".into(),
                "pingap:compressionUpstream".into(),
            ]),
            ..Default::default()
        },
    );
    assert!(
        validate_plugins(
            &config,
            &BTreeSet::from(["request_id".into(), "compression".into()])
        )
        .is_ok()
    );
    assert!(validate_plugins(&config, &BTreeSet::from(["request_id".into()])).is_err());
    config.locations.get_mut("route").unwrap().plugins =
        Some(vec!["pingap:not-a-real-plugin".into()]);
    assert!(
        validate_plugins(
            &config,
            &BTreeSet::from(["request_id".into(), "compression".into()])
        )
        .is_err()
    );
}

// Component-level process fixture, not official binary acceptance. It exposes
// only the real upstream basic shape, so accidentally restoring private flags
// or daemonizing the probe fails the fixture before it can cache a result.
#[cfg(unix)]
fn fixture(directory: &Path, category: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = directory.join("pingap-fixture");
    let script = format!(
        r#"#!/usr/bin/env python3
import hashlib, http.server, json, os, pathlib, sys
if '-t' in sys.argv:
    conf = pathlib.Path(sys.argv[sys.argv.index('-c') + 1]).read_text()
    sys.exit(17 if 'reject_fixture_constructor' in conf else 0)
if len(sys.argv) != 3 or sys.argv[1] != '-c':
    sys.exit(21)
for key in ['PINGAP_DAEMON','PINGAP_UPGRADE','PINGAP_CP','PINGAP_AUTORELOAD','PINGAP_AUTORESTART','PINGAP_READY_SOCK']:
    if os.environ.get(key):
        sys.exit(22)
conf = pathlib.Path(sys.argv[2]).read_text()
if '[servers.' in conf or '[plugins.' in conf or 'grace_period = "0s"' not in conf:
    sys.exit(23)
pathlib.Path({counter}).write_text(str(os.getpid()))
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        value = self.headers.get('Authorization', '')
        try:
            token, ts = value.split(':')
        except ValueError:
            self.send_error(403); return
        expected = hashlib.sha256((os.environ['PINGAP_ADMIN_USER'] + ':' + os.environ['PINGAP_ADMIN_PASSWORD'] + ':' + ts).encode()).hexdigest()
        if token != expected:
            self.send_error(403); return
        body = json.dumps({{'pid':str(os.getpid()), 'version':{version}, 'git_hash':{commit}, 'supported_plugins':[{category}]}}).encode()
        self.send_response(200); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args):
        pass
addr = os.environ['PINGAP_ADMIN_ADDR'].split(':')
http.server.HTTPServer((addr[0],int(addr[1])), Handler).serve_forever()
"#,
        counter = serde_json::to_string(&directory.join("observed-pid").to_string_lossy()).unwrap(),
        version = serde_json::to_string(PINNED_VERSION).unwrap(),
        commit = serde_json::to_string(PINNED_COMMIT).unwrap(),
        category = serde_json::to_string(category).unwrap(),
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn owned_probe_without_private_protocol_is_cached_only_after_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let binary = fixture(directory.path(), "mock");
    let config = PingapConfig::default();
    preflight_until(&binary, &config, Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    let pid: u32 = std::fs::read_to_string(directory.path().join("observed-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(!process_utils::process_exists(pid).unwrap());
    std::fs::remove_file(directory.path().join("observed-pid")).unwrap();
    preflight_until(&binary, &config, Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        !directory.path().join("observed-pid").exists(),
        "cache hit must not start a second probe"
    );
    let mut unsupported = config;
    unsupported.plugins.insert(
        "required".into(),
        toml::from_str("category = 'missing'").unwrap(),
    );
    assert!(
        preflight_until(
            &binary,
            &unsupported,
            Instant::now() + Duration::from_secs(10)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("unavailable")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_plugin_constructor_failure_is_not_replaced_by_admin_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let binary = fixture(directory.path(), "reject_fixture_constructor");
    let mut config = PingapConfig::default();
    config.plugins.insert(
        "bad".into(),
        toml::from_str("category = 'reject_fixture_constructor'").unwrap(),
    );
    let result =
        super::super::compiler::validate_candidate(&binary, &config, directory.path()).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("-t rejected candidate")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_test_exit_zero_cannot_validate_a_replaced_executable() {
    let directory = tempfile::tempdir().unwrap();
    let binary = fixture(directory.path(), "mock");
    let script = std::fs::read_to_string(&binary).unwrap().replace(
        "sys.exit(17 if 'reject_fixture_constructor' in conf else 0)",
        "replacement = pathlib.Path(__file__).with_suffix('.replacement')\n    replacement.write_text('#!/bin/sh\\nexit 0\\n')\n    replacement.chmod(0o700)\n    os.replace(replacement, __file__)\n    print('unavailable plugin category warning')\n    sys.exit(0)",
    );
    std::fs::write(&binary, script).unwrap();
    let mut config = PingapConfig::default();
    config.plugins.insert(
        "required".into(),
        toml::from_str("category='mock'").unwrap(),
    );
    let error = super::super::compiler::validate_candidate(&binary, &config, directory.path())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("proxy executable changed"),
        "a successful native test from a changed binary cannot reuse prior capabilities: {error:#}"
    );
    let replacement_digest = binary_digest(&binary).await.unwrap();
    assert!(
        !cache().lock().unwrap().contains_key(&replacement_digest),
        "the replacement never furnished an owned capability observation"
    );
    let pid: u32 = std::fs::read_to_string(directory.path().join("observed-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(!process_utils::process_exists(pid).unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn consumed_control_budget_cannot_start_another_native_preflight() {
    let directory = tempfile::tempdir().unwrap();
    let binary = fixture(directory.path(), "mock");
    let native_marker = directory.path().join("native-started");
    let script = std::fs::read_to_string(&binary).unwrap().replace(
        "if '-t' in sys.argv:",
        &format!(
            "if '-t' in sys.argv:\n    pathlib.Path({}).write_text('started')",
            serde_json::to_string(&native_marker.to_string_lossy()).unwrap()
        ),
    );
    std::fs::write(&binary, script).unwrap();
    let digest = binary_digest(&binary).await.unwrap();
    cache()
        .lock()
        .unwrap()
        .entry(digest)
        .or_insert_with(|| Arc::new(BTreeSet::from(["mock".into()])));

    // A prior control phase consumed this request's budget. The cached
    // capability result does not authorize a new 30-second native execution.
    let deadline = Instant::now() + Duration::from_millis(100);
    tokio::time::sleep(Duration::from_millis(80)).await;
    let error = super::super::compiler::validate_candidate_until(
        &binary,
        &PingapConfig::default(),
        directory.path(),
        deadline,
    )
    .await
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("budget") || message.contains("deadline"));
    assert!(
        !native_marker.exists(),
        "expired control cannot dispatch -t"
    );
    assert!(!directory.path().join("observed-pid").exists());
}

#[tokio::test]
async fn expired_candidate_deadline_has_no_filesystem_or_spawn_side_effect() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("not-created");
    let error = super::super::compiler::validate_candidate_until(
        &directory.path().join("missing-binary"),
        &PingapConfig::default(),
        &runtime,
        Instant::now(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("deadline exceeded before preflight"));
    assert!(!runtime.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn native_wait_reserves_original_deadline_for_exact_process_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let binary = fixture(directory.path(), "mock");
    let marker = directory.path().join("native-running.json");
    let release = directory.path().join("release-native");
    let script = std::fs::read_to_string(&binary)
        .unwrap()
        .replace("pathlib, sys", "pathlib, sys, time")
        .replace(
            "sys.exit(17 if 'reject_fixture_constructor' in conf else 0)",
            &format!(
                "pathlib.Path({marker}).write_text(json.dumps({{'pid': os.getpid(), 'config': sys.argv[sys.argv.index('-c') + 1]}}))\n    while not pathlib.Path({release}).exists():\n        time.sleep(0.02)\n    sys.exit(0)",
                marker = serde_json::to_string(&marker.to_string_lossy()).unwrap(),
                release = serde_json::to_string(&release.to_string_lossy()).unwrap(),
            ),
        );
    std::fs::write(&binary, script).unwrap();
    let digest = binary_digest(&binary).await.unwrap();
    cache()
        .lock()
        .unwrap()
        .entry(digest)
        .or_insert_with(|| Arc::new(BTreeSet::from(["mock".into()])));
    let runtime = directory.path().to_owned();
    let started = Instant::now();
    let deadline = started + CLEANUP_GRACE + Duration::from_secs(1);
    let task = tokio::spawn(async move {
        super::super::compiler::validate_candidate_until(
            &binary,
            &PingapConfig::default(),
            &runtime,
            deadline,
        )
        .await
    });
    let observation: serde_json::Value = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(&marker).await
                && let Ok(value) = serde_json::from_slice(&bytes)
            {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "native phase must wait on the real barrier"
    );
    let error = task.await.unwrap().unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
            .is_none(),
        "the original budget must retain time to confirm ordinary cleanup: {error:#}"
    );
    assert!(format!("{error:#}").contains("execution deadline exceeded"));
    assert!(Instant::now() < deadline);
    let pid = u32::try_from(observation["pid"].as_u64().unwrap()).unwrap();
    assert!(!process_utils::process_exists(pid).unwrap());
    let config = std::path::PathBuf::from(observation["config"].as_str().unwrap());
    assert!(!config.parent().unwrap().exists());
    assert!(
        !release.exists(),
        "timeout did not release the native barrier"
    );
}

//! RCoder UserApp facade routes must remain reachable through the file entry.
use http_server::router_docs::ApiDoc;
use rcoder_engine::userapp_forward::guard_tables::{APP_MANAGER_PATHS, LOCAL_USERAPP_PATHS};
use shared_types::file_entry_policy::{Method, allows, contains_path};
use std::collections::BTreeSet;
use utoipa::OpenApi;

fn sample_path(template: &str) -> String {
    template
        .split('/')
        .map(|segment| {
            if segment.starts_with("{*") {
                "nested/file.txt"
            } else if segment.starts_with('{') {
                "fixture"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn actual_platform_userapp_documented_methods_remain_public() {
    let document = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let mut documented = BTreeSet::new();
    for (template, item) in document["paths"].as_object().unwrap() {
        if !template.starts_with("/api/v1/userapp/") {
            continue;
        }
        documented.insert(template.replace("{*", "{"));
        let path = sample_path(template);
        for verb in ["get", "post", "put", "patch", "delete", "options", "head"] {
            if item.get(verb).is_none() {
                continue;
            }
            let method = Method::from_bytes(verb.to_uppercase().as_bytes()).unwrap();
            assert!(allows(&method, &path), "missing platform {verb} {template}");
            if method == Method::GET {
                assert!(allows(&Method::HEAD, &path));
            }
        }
    }
    // ApiDoc deliberately omits workspace::create_workspace: its source
    // comment marks this platform creation endpoint as internal documentation.
    // Compare concrete authoritative sets, not an assumed route-count floor.
    let expected: BTreeSet<_> = APP_MANAGER_PATHS
        .iter()
        .chain(LOCAL_USERAPP_PATHS.iter())
        .filter(|path| **path != "/api/v1/userapp/workspace")
        .map(|path| path.replace("{*", "{"))
        .collect();
    assert_eq!(
        documented, expected,
        "ApiDoc UserApp paths differ from the actual route tables"
    );
}

#[test]
fn guard_tables_and_root_redirects_remain_within_the_public_policy() {
    for template in APP_MANAGER_PATHS.iter().chain(LOCAL_USERAPP_PATHS.iter()) {
        assert!(
            contains_path(&sample_path(template)),
            "missing platform path: {template}"
        );
    }
    for template in LOCAL_USERAPP_PATHS
        .iter()
        .filter(|template| template.contains("/proxy/"))
    {
        let root = template.strip_suffix("/{*path}").unwrap();
        assert!(
            allows(&Method::GET, &sample_path(root)),
            "missing proxy root redirect: {root}"
        );
    }
    assert!(allows(&Method::GET, "/api/v1/userapp/app/prod/readiness"));
    assert!(allows(&Method::POST, "/api/v1/userapp/app/restart"));
    assert!(allows(&Method::POST, "/api/v1/userapp/workspace"));
    assert!(allows(&Method::GET, "/api/build/start-dev"));
    assert!(allows(&Method::GET, "/api/build/stop-dev"));
    assert!(allows(&Method::GET, "/api/build/restart-dev"));
    for path in [
        "/api/system/file-server/stop",
        "/api/v1/admin/userapp/error-page",
        "/internal/pod/ensure",
    ] {
        assert!(!contains_path(path));
    }
}

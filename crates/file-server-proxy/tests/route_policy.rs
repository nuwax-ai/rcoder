//! Reverse-check the public leaf policy against actual container OpenAPI.
#![cfg(feature = "embed-file-server")]

use shared_types::file_entry_policy::{Method, allows};

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

fn assert_document(document: serde_json::Value) -> usize {
    let mut checked = 0;
    for (template, item) in document["paths"].as_object().unwrap() {
        let path = sample_path(template);
        for verb in ["get", "post", "put", "patch", "delete", "options", "head"] {
            if item.get(verb).is_none() {
                continue;
            }
            let method = Method::from_bytes(verb.to_uppercase().as_bytes()).unwrap();
            assert!(allows(&method, &path), "missing {verb} {template}");
            if method == Method::GET {
                assert!(
                    allows(&Method::HEAD, &path),
                    "GET must retain HEAD: {template}"
                );
            }
            checked += 1;
        }
    }
    checked
}

#[test]
fn actual_file_server_and_userapp_documents_fit_the_entry_policy() {
    let file = serde_json::to_value(file_server::routes::api_router().into_openapi()).unwrap();
    let userapp = serde_json::to_value(file_server_userapp::document()).unwrap();
    assert!(assert_document(file) > 60);
    assert!(assert_document(userapp) >= 39);
}

#[test]
fn static_options_and_swagger_assets_keep_their_actual_methods() {
    for path in [
        "/api/computer/static/user/computer/nested/file.txt",
        "/api/page/static/project/nested/file.txt",
        "/api/v1/userapp/static/app",
    ] {
        assert!(allows(&Method::GET, path));
        assert!(allows(&Method::HEAD, path));
        assert!(allows(&Method::OPTIONS, path));
        assert!(!allows(&Method::POST, path));
    }
    for path in [
        "/api-docs",
        "/api-docs/",
        "/api-docs/openapi.json",
        "/api-docs/swagger-ui.css",
    ] {
        assert!(allows(&Method::GET, path));
        assert!(allows(&Method::HEAD, path));
        assert!(!allows(&Method::POST, path));
    }
}

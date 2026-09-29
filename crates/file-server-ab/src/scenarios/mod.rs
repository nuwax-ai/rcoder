//! 场景通用 RequestSpec 构造（json/get/multipart）与各套件子模块。

pub(crate) mod core;
pub(crate) mod git;
pub(crate) mod lifecycle;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{Value, json};

use crate::types::*;

pub(crate) fn multipart_form_body(
    boundary: &str,
    text_fields: &[(&str, &str)],
    file_fields: &[(&str, &str, &[u8])],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in text_fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    for (field_name, file_name, bytes) in file_fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_name}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

pub(crate) fn multipart_spec(
    path: impl Into<String>,
    text_fields: &[(&str, &str)],
    file_fields: &[(&str, &str, &[u8])],
) -> RequestSpec {
    RequestSpec {
        method: Method::POST,
        path: path.into(),
        body: multipart_form_body(AB_MULTIPART_BOUNDARY, text_fields, file_fields),
        content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(45),
    }
}

pub(crate) fn json_spec(method: Method, path: &str, body: Value) -> Result<RequestSpec> {
    Ok(RequestSpec {
        method,
        path: path.to_string(),
        body: serde_json::to_vec(&body).context("serialize A/B request body")?,
        content_type: Some("application/json"),
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: if path == "/api/git/commit" {
            vec!["/commit".to_string()]
        } else {
            Vec::new()
        },
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    })
}

pub(crate) fn project_create_spec(project_id: &str, template_type: &str) -> Result<RequestSpec> {
    let mut spec = json_spec(
        Method::POST,
        "/api/project/create-project",
        json!({"projectId": project_id, "templateType": template_type}),
    )?;
    // Template initialization includes extraction, agent metadata synchronization, and a Git
    // commit. Cold Docker volumes can exceed the generic API timeout even when initialization
    // succeeds; allow the comparison to observe the actual result rather than cascade into
    // later scenarios against a project that is still being initialized.
    spec.timeout = Duration::from_secs(120);
    Ok(spec)
}

pub(crate) fn get_spec(path: impl Into<String>) -> RequestSpec {
    RequestSpec {
        method: Method::GET,
        path: path.into(),
        body: Vec::new(),
        content_type: None,
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    }
}

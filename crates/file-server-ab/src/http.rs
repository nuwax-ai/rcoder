//! HTTP 执行层：请求交换、成对执行、健康等待、端口探测。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::{Client, StatusCode, header::HeaderMap};

use crate::compare::*;
use crate::report::*;
use crate::types::*;
use crate::util::*;

// Both implementations can require side-specific dynamic values (pid, assigned ports) while
// keeping the HTTP request path and comparison rules explicit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_pair_specs(
    client: &Client,
    journal: &mut RequestJournal,
    case: &str,
    rust_url: &str,
    ts_url: &str,
    rust_spec: &RequestSpec,
    ts_spec: &RequestSpec,
    target: RequestTarget,
) -> Result<(CaseResult, Exchange, Exchange)> {
    let ts = exchange(client, journal, case, "typescript", ts_url, ts_spec, target).await?;
    let rust = exchange(client, journal, case, "rust", rust_url, rust_spec, target).await?;
    let normalized_paths = rust_spec
        .normalized_paths
        .iter()
        .chain(ts_spec.normalized_paths.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let normalized_headers = rust_spec
        .normalized_headers
        .iter()
        .chain(ts_spec.normalized_headers.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let health_probe = rust_spec.health_probe || ts_spec.health_probe;
    // Static file serving carries browser-facing validator/caching semantics that the
    // header protocol layer enforces more strictly than framework defaults on APIs.
    // Those semantics bind representations (2xx/206/304); on error responses Express's
    // framework-default headers are noise, so strictness only applies when BOTH sides
    // returned a successful representation (a status mismatch is reported separately).
    let is_representation = |exchange: &Exchange| {
        exchange.status.is_some_and(|status| {
            status.is_success()
                || status == StatusCode::PARTIAL_CONTENT
                || status == StatusCode::NOT_MODIFIED
        })
    };
    let static_representation = ["/api/page/static/", "/api/computer/static/"]
        .iter()
        .any(|prefix| rust_spec.path.starts_with(prefix))
        || ["/api/page/static/", "/api/computer/static/"]
            .iter()
            .any(|prefix| ts_spec.path.starts_with(prefix));
    let static_representation =
        static_representation && is_representation(&rust) && is_representation(&ts);
    let differences = compare_exchange(
        case,
        &rust,
        &ts,
        health_probe,
        rust_spec.expected_status,
        &normalized_paths,
        &normalized_headers,
        static_representation,
    );
    let equal = differences.is_empty();
    let case_result = CaseResult {
        case: case.to_string(),
        equal,
        compared: if health_probe {
            "expected HTTP class, HTTP status and JSON status field; runtime metadata is not gated"
                .into()
        } else {
            "expected HTTP class, HTTP status, selected stable headers, and complete response body"
                .into()
        },
        normalized_paths,
        normalized_headers,
        rust_status: rust.status.map(|status| status.as_u16()),
        ts_status: ts.status.map(|status| status.as_u16()),
        differences,
        blocked_by: None,
    };
    Ok((case_result, rust, ts))
}

/// Confirm that a dev-server port is actually closed after stop. An HTTP transport error alone
/// could also mean DNS or another network failure, so require the TCP connect to be refused.
pub(crate) async fn dev_port_accepts_connections(endpoint: &str) -> Result<bool, String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| format!("parse dev-server endpoint: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "dev-server endpoint has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "dev-server endpoint has no port".to_string())?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("resolve dev-server host: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("dev-server hostname resolved to no addresses".into());
    }

    for address in addresses {
        match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(_)) => return Ok(true),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(Err(error)) => return Err(format!("connect to {address}: {error}")),
            Err(_) => return Err(format!("connect to {address} timed out")),
        }
    }
    Ok(false)
}

pub(crate) async fn exchange(
    client: &Client,
    journal: &mut RequestJournal,
    case: &str,
    side: &str,
    base_url: &str,
    spec: &RequestSpec,
    target: RequestTarget,
) -> Result<Exchange> {
    let url = endpoint(base_url, &spec.path);
    let mut request = client.request(spec.method.clone(), &url);
    let mut request_headers = BTreeMap::new();
    if let Some(content_type) = spec.content_type {
        request = request.header("content-type", content_type);
        request_headers.insert("content-type".to_string(), content_type.to_string());
    }
    for (name, value) in &spec.headers {
        request = request.header(name, value);
        request_headers.insert(name.clone(), value.clone());
    }
    if !spec.body.is_empty() {
        request = request.body(spec.body.clone());
    }
    let request_body_sha256 = sha256(&spec.body);
    let (request_body_file, request_body_truncated) = if spec.body.is_empty() {
        (None, false)
    } else {
        let (file, truncated) = save_body(&journal.report_dir, case, side, "request", &spec.body)?;
        (Some(file), truncated)
    };
    let request_id = uuid::Uuid::now_v7().to_string();
    journal.begin(&RequestStartLine {
        request_id: request_id.clone(),
        case: case.to_string(),
        side: side.to_string(),
        method: spec.method.to_string(),
        url: url.clone(),
        started_at: timestamp_rfc3339(),
    })?;
    let started = std::time::Instant::now();
    let response = request.timeout(spec.timeout).send().await;
    let headers_elapsed_ms = response
        .as_ref()
        .ok()
        .map(|_| started.elapsed().as_millis());
    let exchange = match response {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = match response.bytes().await {
                Ok(body) => body.to_vec(),
                Err(error) => {
                    let message = format!("read response body: {error:#}");
                    let record = RequestLine {
                        request_id: request_id.clone(),
                        case: case.to_string(),
                        side: side.to_string(),
                        method: spec.method.to_string(),
                        url: url.clone(),
                        request_headers,
                        request_body_bytes: spec.body.len(),
                        request_body_sha256,
                        request_body_file,
                        status: Some(status.as_u16()),
                        response_headers: headers_map(&headers),
                        response_body_bytes: None,
                        response_body_sha256: None,
                        response_body_file: None,
                        request_body_truncated,
                        response_body_truncated: false,
                        headers_elapsed_ms,
                        elapsed_ms: started.elapsed().as_millis(),
                        transport_error: Some(message.clone()),
                    };
                    journal.append(&record, &spec.path, target)?;
                    return Ok(Exchange {
                        status: Some(status),
                        headers,
                        body: Vec::new(),
                        transport_error: Some(message),
                    });
                }
            };
            let (response_body_file, response_body_truncated) =
                save_body(&journal.report_dir, case, side, "response", &body)?;
            let record = RequestLine {
                request_id: request_id.clone(),
                case: case.to_string(),
                side: side.to_string(),
                method: spec.method.to_string(),
                url: url.clone(),
                request_headers,
                request_body_bytes: spec.body.len(),
                request_body_sha256,
                request_body_file,
                status: Some(status.as_u16()),
                response_headers: headers_map(&headers),
                response_body_bytes: Some(body.len()),
                response_body_sha256: Some(sha256(&body)),
                response_body_file: Some(response_body_file),
                request_body_truncated,
                response_body_truncated,
                headers_elapsed_ms,
                elapsed_ms: started.elapsed().as_millis(),
                transport_error: None,
            };
            journal.append(&record, &spec.path, target)?;
            Exchange {
                status: Some(status),
                headers,
                body,
                transport_error: None,
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let record = RequestLine {
                request_id: request_id.clone(),
                case: case.to_string(),
                side: side.to_string(),
                method: spec.method.to_string(),
                url,
                request_headers,
                request_body_bytes: spec.body.len(),
                request_body_sha256,
                request_body_file,
                status: None,
                response_headers: BTreeMap::new(),
                response_body_bytes: None,
                response_body_sha256: None,
                response_body_file: None,
                request_body_truncated,
                response_body_truncated: false,
                headers_elapsed_ms,
                elapsed_ms: started.elapsed().as_millis(),
                transport_error: Some(message.clone()),
            };
            journal.append(&record, &spec.path, target)?;
            Exchange {
                status: None,
                headers: HeaderMap::new(),
                body: Vec::new(),
                transport_error: Some(message),
            }
        }
    };
    Ok(exchange)
}

pub(crate) async fn wait_health(client: &Client, base_url: &str, side: &str) -> Result<()> {
    let until = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    let url = endpoint(base_url, "/health");
    loop {
        let last_error = match client
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                println!("{side} healthy");
                return Ok(());
            }
            Ok(response) => format!("HTTP {}", response.status()),
            Err(error) => error.to_string(),
        };
        if tokio::time::Instant::now() < until {
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        } else {
            bail!("{side} health timed out at {url}; last error: {last_error}");
        }
    }
}

pub(crate) fn save_body(
    report_dir: &Path,
    case: &str,
    side: &str,
    kind: &str,
    bytes: &[u8],
) -> Result<(String, bool)> {
    let safe_case = case
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let path = PathBuf::from("bodies").join(format!("{safe_case}-{side}-{kind}.bin"));
    let full = report_dir.join(&path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    let truncated = bytes.len() > BODY_CAPTURE_LIMIT;
    fs::write(&full, &bytes[..bytes.len().min(BODY_CAPTURE_LIMIT)])?;
    Ok((path.to_string_lossy().into_owned(), truncated))
}

pub(crate) fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

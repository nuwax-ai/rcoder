//! Protocol boundary for formal UserApp APIs; legacy TS and byte streams keep their wire contract.
use axum::{
    body::to_bytes,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

pub fn is_formal_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/userapp/") else {
        return false;
    };
    if rest.starts_with("static/") || rest.starts_with("proxy/") || rest.starts_with("app-files/") {
        return false;
    }
    !matches!(
        rest,
        "get-file-list"
            | "resolve-file"
            | "search-files"
            | "files-update"
            | "upload-file"
            | "upload-files"
            | "generate-file"
            | "import-project"
            | "execute-command"
            | "get-logs"
            | "zip-workspace"
            | "download-all-files"
            | "init-project-template"
            | "push-skills-to-workspace"
    )
}

pub async fn envelope_errors(request: Request, next: Next) -> Response {
    let formal = is_formal_path(request.uri().path());
    let response = next.run(request).await;
    if !formal || !(response.status().is_client_error() || response.status().is_server_error()) {
        return response;
    }
    let status = response.status();
    let fallback = if status.is_client_error() {
        crate::error_codes::ERR_VALIDATION
    } else {
        crate::error_codes::ERR_BACKEND_ERROR
    };
    let (mut parts, body) = response.into_parts();
    let parsed = match to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
        Err(_) => None,
    };
    let code = parsed
        .as_ref()
        .and_then(|v| v["code"].as_str())
        .filter(|c| c.starts_with("ERR_"))
        .unwrap_or(fallback);
    let message = parsed
        .as_ref()
        .and_then(|v| v["message"].as_str())
        .filter(|m| m.is_ascii())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::error_codes::get_error_message(code, "en-US"));
    let normalized = axum::Json(crate::HttpResult::<()>::error(code, &message)).into_response();
    parts.status = StatusCode::OK;
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, normalized.into_body())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_and_stream_contracts_are_explicit() {
        for path in [
            "get-file-list",
            "execute-command",
            "static/app",
            "proxy/app/dev/u/a/x",
            "app-files/list",
        ] {
            assert!(!is_formal_path(&format!("/api/v1/userapp/{path}")));
        }
        for path in [
            "build",
            "ensure-workspace",
            "tasks/id/cancel",
            "a/dev/install-project",
            "a/start",
        ] {
            assert!(is_formal_path(&format!("/api/v1/userapp/{path}")));
        }
    }
    #[tokio::test]
    async fn formal_extractor_errors_are_enveloped_and_legacy_status_is_preserved() {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route(
                "/api/v1/userapp/build",
                axum::routing::post(|_: axum::Json<serde_json::Value>| async {
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/api/v1/userapp/execute-command",
                axum::routing::post(|| async { StatusCode::BAD_REQUEST }),
            )
            .layer(axum::middleware::from_fn(envelope_errors));
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/userapp/build")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{"))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(body["code"], crate::error_codes::ERR_VALIDATION);
        assert_eq!(body["success"], false);
        assert!(body["message"].as_str().expect("message").is_ascii());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/userapp/execute-command")
                    .body(axum::body::Body::empty())
                    .expect("legacy request"),
            )
            .await
            .expect("legacy response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

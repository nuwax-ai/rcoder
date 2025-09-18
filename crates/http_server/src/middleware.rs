use axum::{
    body::Body,
    extract::{MatchedPath, Request},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    RequestExt,
};
use tower_http::trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::{debug, error, info, span, Level, Span};

pub fn tracing_middleware() -> TraceLayer<DefaultOnRequest, DefaultOnResponse, DefaultOnFailure> {
    TraceLayer::new_for_http()
        .on_request(|request: &Request<Body>, _span: &Span| {
            debug!(
                method = %request.method(),
                uri = %request.uri(),
                "started processing request"
            );
        })
        .on_response(|response: &Response, latency: std::time::Duration, _span: &Span| {
            debug!(
                status = %response.status(),
                latency = ?latency,
                "finished processing request"
            );
        })
        .on_failure(|error: tower::BoxError, _latency: std::time::Duration, _span: &Span| {
            error!("request failed: {}", error);
        })
}

#[derive(Clone)]
pub struct AuthMiddleware {
    // Add authentication state here
}

impl AuthMiddleware {
    pub fn new() -> Self {
        Self {}
    }
}

// Simplified middleware implementation
impl AuthMiddleware {
    pub async fn from_request() -> Result<Self, StatusCode> {
        // For now, we'll allow all requests
        // In a real implementation, you would validate authentication tokens
        Ok(Self::new())
    }
}
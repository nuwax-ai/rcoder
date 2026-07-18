//! HTTP 提取器适配层：把 Axum 原生 rejection 统一映射为 file-server `AppError`。

use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(request, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid JSON request: {error}")))
    }
}

impl<T> IntoResponse for AppJson<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

pub struct AppQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid query parameters: {error}")))
    }
}

pub struct AppPath<T>(pub T);

impl<S, T> FromRequestParts<S> for AppPath<T>
where
    S: Send + Sync,
    Path<T>: FromRequestParts<S, Rejection = PathRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid path parameters: {error}")))
    }
}

pub struct AppMultipart(Multipart);

impl<S> FromRequest<S> for AppMultipart
where
    S: Send + Sync,
    Multipart: FromRequest<S, Rejection = MultipartRejection>,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Multipart::from_request(request, state)
            .await
            .map(Self)
            .map_err(|error| AppError::validation(format!("invalid multipart request: {error}")))
    }
}

impl std::ops::Deref for AppMultipart {
    type Target = Multipart;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for AppMultipart {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use serde::Deserialize;
    use tower::ServiceExt;

    use super::AppJson;

    #[derive(Deserialize)]
    struct Input {
        value: String,
    }

    async fn handler(AppJson(input): AppJson<Input>) -> AppJson<serde_json::Value> {
        AppJson(serde_json::json!({ "success": true, "value": input.value }))
    }

    #[tokio::test]
    async fn malformed_json_uses_unified_error_response() {
        let app = Router::new().route("/", post(handler));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("{invalid"))
            .expect("valid request fixture");

        let response = app.oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("error response body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON error response");
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["type"], "VALIDATION_ERROR");
    }
}

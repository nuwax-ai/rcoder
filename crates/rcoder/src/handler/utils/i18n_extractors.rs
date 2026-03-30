//! Locale-aware request extractors.
//!
//! These wrappers map Axum extractor rejections into unified `AppError` so
//! HTTP error responses are consistently localized through `AppError::into_response`.

use axum::{
    extract::{
        FromRequest, FromRequestParts, Json, Path, Query, Request,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::request::Parts,
};

use crate::AppError;

/// Locale-aware JSON extractor.
pub struct I18nJson<T>(pub T);

impl<S, T> FromRequest<S> for I18nJson<T>
where
    S: Send + Sync,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_json_rejection(rejection)),
        }
    }
}

/// Locale-aware query extractor.
pub struct I18nQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for I18nQuery<T>
where
    S: Send + Sync,
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_query_rejection(rejection)),
        }
    }
}

/// Locale-aware path extractor.
pub struct I18nPath<T>(pub T);

impl<S, T> FromRequestParts<S> for I18nPath<T>
where
    S: Send + Sync,
    Path<T>: FromRequestParts<S, Rejection = PathRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_path_rejection(rejection)),
        }
    }
}

fn map_json_rejection(rejection: JsonRejection) -> AppError {
    use shared_types::error_codes::{ERR_INVALID_PARAMS, ERR_VALIDATION};

    match rejection {
        JsonRejection::JsonDataError(_) => {
            AppError::with_i18n_key(ERR_VALIDATION, "error.validation")
        }
        JsonRejection::JsonSyntaxError(_)
        | JsonRejection::MissingJsonContentType(_)
        | JsonRejection::BytesRejection(_) => {
            AppError::with_i18n_key(ERR_INVALID_PARAMS, "error.invalid_params")
        }
        _ => AppError::with_i18n_key(ERR_INVALID_PARAMS, "error.invalid_params"),
    }
}

fn map_query_rejection(_rejection: QueryRejection) -> AppError {
    AppError::with_i18n_key(shared_types::error_codes::ERR_INVALID_PARAMS, "error.invalid_params")
}

fn map_path_rejection(_rejection: PathRejection) -> AppError {
    AppError::with_i18n_key(shared_types::error_codes::ERR_INVALID_PARAMS, "error.invalid_params")
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use serde::Deserialize;
    use tower::ServiceExt;

    use super::{I18nJson, I18nPath, I18nQuery};

    #[derive(Deserialize)]
    struct Payload {
        value: i32,
    }

    #[derive(Deserialize)]
    struct QueryParams {
        limit: u32,
    }

    async fn json_handler(I18nJson(payload): I18nJson<Payload>) -> impl IntoResponse {
        payload.value.to_string()
    }

    async fn query_handler(I18nQuery(params): I18nQuery<QueryParams>) -> impl IntoResponse {
        params.limit.to_string()
    }

    async fn path_handler(I18nPath(id): I18nPath<u32>) -> impl IntoResponse {
        id.to_string()
    }

    #[tokio::test]
    async fn test_i18n_json_rejection_returns_bad_request() {
        let app = Router::new().route("/json", post(json_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/json")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"value":"oops"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_query_rejection_returns_bad_request() {
        let app = Router::new().route("/query", get(query_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/query?limit=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_path_rejection_returns_bad_request() {
        let app = Router::new().route("/path/{id}", get(path_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/path/not-a-number")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

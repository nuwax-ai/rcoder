//! Formal userApp extractors: rejection uses the same business envelope as handlers.
use crate::UserAppError;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;

pub struct AppJson<T>(pub T);
impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    file_server::extract::AppJson<T>: FromRequest<S, Rejection = file_server::error::AppError>,
{
    type Rejection = UserAppError;
    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        file_server::extract::AppJson::<T>::from_request(request, state)
            .await
            .map(|file_server::extract::AppJson(value)| Self(value))
            .map_err(Into::into)
    }
}

pub struct AppQuery<T>(pub T);
impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    file_server::extract::AppQuery<T>:
        FromRequestParts<S, Rejection = file_server::error::AppError>,
{
    type Rejection = UserAppError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        file_server::extract::AppQuery::<T>::from_request_parts(parts, state)
            .await
            .map(|file_server::extract::AppQuery(value)| Self(value))
            .map_err(Into::into)
    }
}

pub struct AppPath<T>(pub T);
impl<S, T> FromRequestParts<S> for AppPath<T>
where
    S: Send + Sync,
    file_server::extract::AppPath<T>: FromRequestParts<S, Rejection = file_server::error::AppError>,
{
    type Rejection = UserAppError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        file_server::extract::AppPath::<T>::from_request_parts(parts, state)
            .await
            .map(|file_server::extract::AppPath(value)| Self(value))
            .map_err(Into::into)
    }
}

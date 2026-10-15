//! Shared request intent for the developer file-server forwarding surface.
use axum::http::Method;

use crate::userapp_builder::BuilderAccessIntent;

pub(super) fn builder_access_intent(method: &Method, path: &str) -> BuilderAccessIntent {
    if method == Method::POST
        && matches!(
            path,
            "/api/v1/userapp/build" | "/api/v1/userapp/dev/start" | "/api/v1/userapp/dev/restart"
        )
    {
        BuilderAccessIntent::SourceExecution
    } else {
        BuilderAccessIntent::Management
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_source_requests_carry_business_intent() {
        for path in [
            "/api/v1/userapp/build",
            "/api/v1/userapp/dev/start",
            "/api/v1/userapp/dev/restart",
        ] {
            assert_eq!(
                builder_access_intent(&Method::POST, path),
                BuilderAccessIntent::SourceExecution
            );
            assert_eq!(
                builder_access_intent(&Method::GET, path),
                BuilderAccessIntent::Management
            );
        }
        for (method, path) in [
            (Method::GET, "/api/computer/get-file-list"),
            (Method::GET, "/api/git/status"),
            (Method::POST, "/api/git/commit"),
            (Method::POST, "/api/v1/userapp/ensure-workspace"),
            (Method::POST, "/api/v1/userapp/dev/stop"),
            (Method::GET, "/api/v1/userapp/tasks/task1/logs/stream"),
        ] {
            assert_eq!(
                builder_access_intent(&method, path),
                BuilderAccessIntent::Management
            );
        }
    }
}

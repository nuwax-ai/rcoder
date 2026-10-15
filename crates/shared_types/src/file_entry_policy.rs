//! Public file-entry methods and paths. The same leaf contract is used by
//! embedded and network proxies; a service header selects an implementation,
//! never an additional host API. Keep route/OpenAPI reverse checks in sync.

pub use axum::http::Method;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

impl Verb {
    pub fn method(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
            Self::Options => Method::OPTIONS,
        }
    }
    fn allows(self, method: &Method) -> bool {
        self.method() == *method || (self == Self::Get && *method == Method::HEAD)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Route {
    pub path: &'static str,
    pub verb: Verb,
}

impl Route {
    const fn new(path: &'static str, verb: Verb) -> Self {
        Self { path, verb }
    }
}

/// Registered file-server/container routes, RCoder UserApp facade routes,
/// documented UserApp proxy roots, and actual Swagger/static methods.
/// Wildcard tails are limited to explicitly registered static/proxy families.
pub const ROUTES: &[Route] = &[
    Route::new("/", Verb::Get),
    Route::new("/api-docs", Verb::Get),
    Route::new("/api-docs/", Verb::Get),
    Route::new("/api-docs/openapi.json", Verb::Get),
    Route::new("/api-docs/{*rest}", Verb::Get),
    Route::new("/api/build/build", Verb::Get),
    Route::new("/api/build/clear-all-log-cache", Verb::Get),
    Route::new("/api/build/get-dev-log", Verb::Get),
    Route::new("/api/build/get-log-cache-stats", Verb::Get),
    Route::new("/api/build/keep-alive", Verb::Get),
    Route::new("/api/build/list-dev", Verb::Get),
    Route::new("/api/build/parse-build-error", Verb::Post),
    Route::new("/api/build/port-pool-status", Verb::Get),
    Route::new("/api/build/restart-dev", Verb::Get),
    Route::new("/api/build/start-dev", Verb::Get),
    Route::new("/api/build/stop-dev", Verb::Get),
    Route::new("/api/computer/build-agent-package", Verb::Post),
    Route::new("/api/computer/cleanup-build-artifacts", Verb::Post),
    Route::new("/api/computer/create-workspace", Verb::Post),
    Route::new("/api/computer/create-workspace-v2", Verb::Post),
    Route::new("/api/computer/delete-workspace", Verb::Post),
    Route::new("/api/computer/download-all-files", Verb::Get),
    Route::new("/api/computer/execute-command", Verb::Post),
    Route::new("/api/computer/files-update", Verb::Post),
    Route::new("/api/computer/fs/children", Verb::Get),
    Route::new("/api/computer/fs/mkdir", Verb::Post),
    Route::new("/api/computer/fs/rename", Verb::Post),
    Route::new("/api/computer/fs/roots", Verb::Get),
    Route::new("/api/computer/generate-file", Verb::Post),
    Route::new("/api/computer/get-file-list", Verb::Get),
    Route::new("/api/computer/get-file-meta", Verb::Post),
    Route::new("/api/computer/get-logs", Verb::Get),
    Route::new("/api/computer/import-project", Verb::Post),
    Route::new("/api/computer/init-project-template", Verb::Post),
    Route::new("/api/computer/install-project", Verb::Post),
    Route::new("/api/computer/push-skills-to-workspace", Verb::Post),
    Route::new("/api/computer/push-skills-to-workspace-v2", Verb::Post),
    Route::new("/api/computer/resolve-file", Verb::Get),
    Route::new("/api/computer/search-files", Verb::Get),
    Route::new("/api/computer/static/{user_id}/{c_id}/{*rest}", Verb::Get),
    Route::new(
        "/api/computer/static/{user_id}/{c_id}/{*rest}",
        Verb::Options,
    ),
    Route::new("/api/computer/upload-file", Verb::Post),
    Route::new("/api/computer/upload-files", Verb::Post),
    Route::new("/api/computer/zip-workspace", Verb::Post),
    Route::new("/api/git/add", Verb::Post),
    Route::new("/api/git/branch-create", Verb::Post),
    Route::new("/api/git/branch-delete", Verb::Post),
    Route::new("/api/git/branch-switch", Verb::Post),
    Route::new("/api/git/branches", Verb::Get),
    Route::new("/api/git/checkout", Verb::Post),
    Route::new("/api/git/commit", Verb::Post),
    Route::new("/api/git/diff", Verb::Post),
    Route::new("/api/git/discard", Verb::Post),
    Route::new("/api/git/file-content", Verb::Post),
    Route::new("/api/git/init", Verb::Post),
    Route::new("/api/git/log", Verb::Get),
    Route::new("/api/git/reset", Verb::Post),
    Route::new("/api/git/revert", Verb::Post),
    Route::new("/api/git/status", Verb::Get),
    Route::new("/api/git/tag-create", Verb::Post),
    Route::new("/api/git/tag-delete", Verb::Post),
    Route::new("/api/git/tags", Verb::Get),
    Route::new("/api/git/unstage", Verb::Post),
    Route::new("/api/page/static/{project_id}/{*rest}", Verb::Get),
    Route::new("/api/page/static/{project_id}/{*rest}", Verb::Options),
    Route::new("/api/project/all-files-update", Verb::Post),
    Route::new("/api/project/backup-current-version", Verb::Post),
    Route::new("/api/project/copy-project", Verb::Post),
    Route::new("/api/project/create-project", Verb::Post),
    Route::new("/api/project/delete-project", Verb::Get),
    Route::new("/api/project/export-project", Verb::Post),
    Route::new("/api/project/get-project-content", Verb::Get),
    Route::new("/api/project/get-project-content-by-version", Verb::Get),
    Route::new("/api/project/push-skills-to-workspace", Verb::Post),
    Route::new("/api/project/rollback-version", Verb::Post),
    Route::new("/api/project/specified-files-update", Verb::Post),
    Route::new("/api/project/upload-attachment-file", Verb::Post),
    Route::new("/api/project/upload-batch-files", Verb::Post),
    Route::new("/api/project/upload-project", Verb::Post),
    Route::new("/api/project/upload-single-file", Verb::Post),
    Route::new("/api/v1/userapp/app-files/clear", Verb::Post),
    Route::new("/api/v1/userapp/app-files/clear-target", Verb::Get),
    Route::new("/api/v1/userapp/app-files/delete", Verb::Post),
    Route::new("/api/v1/userapp/app-files/list", Verb::Get),
    Route::new("/api/v1/userapp/app-files/upload", Verb::Post),
    Route::new("/api/v1/userapp/app-files/upload-from-url", Verb::Post),
    Route::new("/api/v1/userapp/build", Verb::Post),
    Route::new("/api/v1/userapp/db/{app_stage}/create-database", Verb::Post),
    Route::new("/api/v1/userapp/db/{app_stage}/reset-password", Verb::Post),
    Route::new(
        "/api/v1/userapp/db/{app_stage}/reset-password/recover",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/deploy-pg/recover", Verb::Post),
    Route::new("/api/v1/userapp/dev/framework-info", Verb::Get),
    Route::new("/api/v1/userapp/dev/list", Verb::Get),
    Route::new(
        "/api/v1/userapp/dev/operations/{operation_id}/recover",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/dev/restart", Verb::Post),
    Route::new("/api/v1/userapp/dev/start", Verb::Post),
    Route::new("/api/v1/userapp/dev/stop", Verb::Post),
    Route::new("/api/v1/userapp/download-all-files", Verb::Get),
    Route::new("/api/v1/userapp/ensure-workspace", Verb::Post),
    Route::new("/api/v1/userapp/execute-command", Verb::Post),
    Route::new("/api/v1/userapp/files-update", Verb::Post),
    Route::new("/api/v1/userapp/generate-file", Verb::Post),
    Route::new("/api/v1/userapp/get-file-list", Verb::Get),
    Route::new("/api/v1/userapp/get-file-meta", Verb::Post),
    Route::new("/api/v1/userapp/get-logs", Verb::Get),
    Route::new("/api/v1/userapp/import-project", Verb::Post),
    Route::new("/api/v1/userapp/init-project-template", Verb::Post),
    Route::new(
        "/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}/{*path}",
        Verb::Get,
    ),
    Route::new("/api/v1/userapp/push-skills-to-workspace", Verb::Post),
    Route::new("/api/v1/userapp/query", Verb::Post),
    Route::new("/api/v1/userapp/resolve-file", Verb::Get),
    Route::new("/api/v1/userapp/runtime", Verb::Get),
    Route::new("/api/v1/userapp/search-files", Verb::Get),
    Route::new("/api/v1/userapp/static/{app_id}", Verb::Get),
    Route::new("/api/v1/userapp/static/{app_id}", Verb::Options),
    Route::new("/api/v1/userapp/storage/{app_stage}/query", Verb::Post),
    Route::new("/api/v1/userapp/tasks/{task_id}", Verb::Get),
    Route::new("/api/v1/userapp/tasks/{task_id}/cancel", Verb::Post),
    Route::new("/api/v1/userapp/tasks/{task_id}/logs/stream", Verb::Get),
    Route::new("/api/v1/userapp/upload-file", Verb::Post),
    Route::new("/api/v1/userapp/upload-files", Verb::Post),
    Route::new("/api/v1/userapp/workspace", Verb::Post),
    Route::new("/api/v1/userapp/zip-workspace", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}", Verb::Get),
    Route::new("/api/v1/userapp/{app_id}/builder/adopt", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/delete/app", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/dev/logs/query", Verb::Post),
    Route::new(
        "/api/v1/userapp/{app_id}/dev/logs/sources/query",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/dev/logs/stream", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/lifecycle", Verb::Get),
    Route::new("/api/v1/userapp/{app_id}/operations/by-request", Verb::Get),
    Route::new("/api/v1/userapp/{app_id}/operations/current", Verb::Get),
    Route::new(
        "/api/v1/userapp/{app_id}/operations/{operation_id}",
        Verb::Get,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/operations/{operation_id}/retry",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/prod/adopt", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/recreate", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/restart", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/start", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/stop", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/update", Verb::Post),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/dbx/readiness",
        Verb::Get,
    ),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/delete", Verb::Post),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/events", Verb::Get),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/files", Verb::Get),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/files/delete",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/health", Verb::Get),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/install-project",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/logs/sources/query",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/projects/confirm",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/readiness", Verb::Get),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/recycle-policy",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/stats", Verb::Get),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/storage", Verb::Get),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/storage/clear",
        Verb::Post,
    ),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/storage/destroy",
        Verb::Post,
    ),
    Route::new("/api/v1/userapp/{app_id}/{app_stage}/upload", Verb::Post),
    Route::new(
        "/api/v1/userapp/{app_id}/{app_stage}/upload-from-url",
        Verb::Post,
    ),
    Route::new("/api/version", Verb::Get),
    Route::new("/health", Verb::Get),
];

pub fn contains_path(path: &str) -> bool {
    ROUTES
        .iter()
        .any(|route| template_matches(route.path, path))
}

/// Literal segments win over parameters, as in the real router. For example,
/// GET /userapp/{app_id} must not authorize GET /userapp/build (POST only).
pub fn allows(method: &Method, path: &str) -> bool {
    let mut best: Option<&str> = None;
    let mut permitted = false;
    for route in ROUTES {
        if !template_matches(route.path, path) {
            continue;
        }
        match best {
            None => {
                best = Some(route.path);
                permitted = route.verb.allows(method);
            }
            Some(template) => match compare_specificity(route.path, template) {
                Ordering::Greater => {
                    best = Some(route.path);
                    permitted = route.verb.allows(method);
                }
                Ordering::Equal => permitted |= route.verb.allows(method),
                Ordering::Less => {}
            },
        }
    }
    permitted
}

fn compare_specificity(left: &str, right: &str) -> Ordering {
    fn rank(segment: &str) -> u8 {
        if segment.starts_with("{*") && segment.ends_with('}') {
            0
        } else if segment.starts_with('{') && segment.ends_with('}') {
            1
        } else {
            2
        }
    }
    left.split('/')
        .skip(1)
        .map(rank)
        .cmp(right.split('/').skip(1).map(rank))
}

fn template_matches(template: &str, path: &str) -> bool {
    if !path.starts_with('/') || path.contains('?') {
        return false;
    }
    let mut expected = template.split('/').skip(1).peekable();
    let mut actual = path.split('/').skip(1);
    while let Some(segment) = expected.next() {
        if segment.starts_with("{*") && segment.ends_with('}') {
            return expected.peek().is_none() && actual.next().is_some();
        }
        let Some(value) = actual.next() else {
            return false;
        };
        if segment.starts_with('{') && segment.ends_with('}') {
            if value.is_empty() {
                return false;
            }
        } else if segment != value {
            return false;
        }
    }
    actual.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_and_literal_precedence_match_the_registered_contract() {
        assert!(allows(&Method::POST, "/api/v1/userapp/build"));
        assert!(!allows(&Method::GET, "/api/v1/userapp/build"));
        assert!(allows(&Method::GET, "/api/v1/userapp/fixture"));
        assert!(allows(&Method::HEAD, "/api/v1/userapp/fixture"));
        assert!(allows(
            &Method::POST,
            "/api/v1/userapp/fixture/prod/logs/stream"
        ));
        assert!(!allows(
            &Method::GET,
            "/api/v1/userapp/fixture/prod/logs/stream"
        ));
        assert!(allows(
            &Method::GET,
            "/api/v1/userapp/fixture/prod/readiness"
        ));
        assert!(allows(&Method::POST, "/api/build/parse-build-error"));
        assert!(allows(&Method::GET, "/api/build/start-dev"));
    }

    #[test]
    fn wildcards_and_options_are_confined_to_explicit_routes() {
        assert!(allows(
            &Method::GET,
            "/api/computer/static/user/computer/css/main.css"
        ));
        assert!(allows(
            &Method::OPTIONS,
            "/api/computer/static/user/computer/css/main.css"
        ));
        assert!(allows(&Method::OPTIONS, "/api/v1/userapp/static/fixture"));
        assert!(!allows(
            &Method::OPTIONS,
            "/api/v1/userapp/fixture/prod/readiness"
        ));
        assert!(!allows(
            &Method::POST,
            "/api/page/static/fixture/index.html"
        ));
        assert!(allows(&Method::GET, "/api-docs/"));
        assert!(allows(&Method::GET, "/api-docs/swagger-ui.css"));
        assert!(!contains_path("/api-docs-other/internal"));
        for path in [
            "/internal/pod/ensure",
            "/api/system/file-server/stop",
            "/api/v1/admin/userapp/error-page",
            "/api/computer/unknown",
            "/api/v1/userapp/fixture/unknown",
        ] {
            assert!(!contains_path(path), "unexpected public route {path}");
        }
    }
}

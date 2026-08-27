//! utoipa 注解式 OpenAPI 文档与内嵌 Swagger UI。
//!
//! 路径、参数和请求体由各 Axum handler 的 `#[utoipa::path]` 提供；
//! `utoipa_axum::OpenApiRouter` 在注册 handler 时同步收集文档，避免维护第二套路由表。

use serde::Serialize;
use serde_json::Value;
use utoipa::{IntoResponses, OpenApi, ToSchema};
use utoipa_swagger_ui::{Config, SwaggerUi};

/// OpenAPI multipart binary item，支持单文件和文件数组。
#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct BinaryFile(String);

/// JSON 成功响应的公共字段。具体接口会附加各自业务字段。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuccessResponse {
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetail {
    pub r#type: String,
    pub message: String,
    pub timestamp: String,
    pub request_id: String,
    #[schema(value_type = Object)]
    pub details: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub success: bool,
    pub code: String,
    pub error: ErrorDetail,
}

/// JSON 接口的公共 HTTP 响应集合。
#[derive(IntoResponses)]
pub enum JsonApiResponses {
    #[response(status = 200, description = "Successful response")]
    Ok(SuccessResponse),
    #[response(status = 400, description = "Validation or business error")]
    BadRequest(ErrorResponse),
    #[response(status = 403, description = "Permission error")]
    Forbidden(ErrorResponse),
    #[response(status = 404, description = "Resource not found")]
    NotFound(ErrorResponse),
    #[response(status = 500, description = "System, file, or process error")]
    InternalServerError(ErrorResponse),
    #[response(status = 502, description = "Network error")]
    BadGateway(ErrorResponse),
}

/// 文件流接口只复用错误响应，成功媒体类型由 handler 单独声明。
#[derive(IntoResponses)]
pub enum ErrorApiResponses {
    #[response(status = 400, description = "Validation or business error")]
    BadRequest(ErrorResponse),
    #[response(status = 403, description = "Permission error")]
    Forbidden(ErrorResponse),
    #[response(status = 404, description = "Resource not found")]
    NotFound(ErrorResponse),
    #[response(status = 500, description = "System, file, or process error")]
    InternalServerError(ErrorResponse),
    #[response(status = 502, description = "Network error")]
    BadGateway(ErrorResponse),
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "RCoder File Server API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Rust file-server API, compatible with nuwax-file-server."
    ),
    servers((url = "/", description = "Current file-server")),
    tags(
        (name = "System", description = "Service health"),
        (name = "Project", description = "Project lifecycle and content"),
        (name = "Code", description = "Project source updates"),
        (name = "Build", description = "Build and Vite dev-server lifecycle"),
        (name = "Git", description = "Git repository operations"),
        (name = "Computer", description = "Computer workspace operations"),
        (name = "Static", description = "Project and workspace static files")
    )
)]
struct ApiMetadata;

/// 合并注解收集的路径与全局 API 元数据。
pub fn document(routes: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    let mut document = ApiMetadata::openapi();
    document.merge(routes);
    normalize_axum_catch_all_paths(&mut document);
    document
}

/// Axum catch-all 写作 `{*rest}`，OpenAPI path template 必须写作 `{rest}`。
fn normalize_axum_catch_all_paths(document: &mut utoipa::openapi::OpenApi) {
    let paths = std::mem::take(&mut document.paths.paths);
    document.paths.paths = paths
        .into_iter()
        .map(|(path, item)| (path.replace("{*", "{"), item))
        .collect();
}

/// 创建内嵌 Swagger UI。静态资源编译进二进制，不依赖运行时 CDN。
pub fn swagger_ui(routes: utoipa::openapi::OpenApi) -> SwaggerUi {
    SwaggerUi::new("/api-docs")
        .url("/api-docs/openapi.json", document(routes))
        .config(Config::new(["/api-docs/openapi.json"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated_document() -> utoipa::openapi::OpenApi {
        document(crate::routes::api_router().into_openapi())
    }

    #[test]
    fn document_contains_every_registered_operation() {
        let document = generated_document();
        // 71 = TS 对齐域全量（镜像族+build+dev）。userApp 域 33 条已拆至
        // file-server-userapp crate（其 routes.rs 测试守卫）。路由增删须同步本计数
        // （防"注册了但没进文档"回归）。
        assert_eq!(document.paths.paths.len(), 71);
        assert!(document.paths.paths.contains_key("/"));
        assert!(document.paths.paths.contains_key("/api/build/start-dev"));
        assert!(document.paths.paths.contains_key("/api/git/commit"));
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/computer/create-workspace-v2")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/computer/generate-file")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/computer/resolve-file")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/computer/search-files")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/page/static/{project_id}/{rest}")
        );
        // userApp 域已拆出：本文档不得再出现 /api/userapp 路径（防回流）
        assert!(
            !document
                .paths
                .paths
                .keys()
                .any(|path| path.starts_with("/api/userapp"))
        );
        assert!(document.paths.paths.keys().all(|path| !path.contains("{*")));
    }

    fn operations_of(
        item: &utoipa::openapi::PathItem,
    ) -> Vec<(&'static str, &utoipa::openapi::path::Operation)> {
        fn push<'a>(
            ops: &mut Vec<(&'static str, &'a utoipa::openapi::path::Operation)>,
            method: &'static str,
            op: &'a Option<utoipa::openapi::path::Operation>,
        ) {
            if let Some(op) = op {
                ops.push((method, op));
            }
        }
        let mut ops = Vec::new();
        push(&mut ops, "get", &item.get);
        push(&mut ops, "post", &item.post);
        push(&mut ops, "put", &item.put);
        push(&mut ops, "delete", &item.delete);
        push(&mut ops, "options", &item.options);
        push(&mut ops, "head", &item.head);
        push(&mut ops, "patch", &item.patch);
        push(&mut ops, "trace", &item.trace);
        ops
    }

    /// operation summary 适配文档 UI（Scalar 左侧菜单与详情区标题显示它）：
    /// 非空、单行、≤50 字符、不带 `` `GET ...`` 方法/路径前缀（UI 已单独显示
    /// method+path）。utoipa 取 doc comment 首段为 summary——首段写长文在此报红。
    #[test]
    fn operation_summaries_are_ui_concise() {
        let document = generated_document();
        const METHODS: [&str; 8] = [
            "GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD", "TRACE",
        ];
        let mut checked = 0usize;
        for (path, item) in &document.paths.paths {
            for (method, op) in operations_of(item) {
                let Some(summary) = op.summary.as_deref().filter(|s| !s.trim().is_empty()) else {
                    panic!("{method} {path}: summary 缺失（doc comment 首段必填）");
                };
                assert!(
                    !summary.contains('\n'),
                    "{method} {path}: summary 须为单行（多行内容移到空行后的详细段）"
                );
                assert!(
                    summary.chars().count() <= 50,
                    "{method} {path}: summary 过长（>50 字符），详细内容移入 description: {summary}"
                );
                let method_prefixed =
                    summary.starts_with('`') && METHODS.iter().any(|m| summary[1..].starts_with(m));
                assert!(
                    !method_prefixed,
                    "{method} {path}: summary 不得带方法/路径前缀（UI 已单独显示）: {summary}"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 60,
            "sanity: 至少遍历 60 个 operation，实际 {checked}"
        );
    }

    // userapp 字段文档/HttpResult 信封两测试已随域迁至 file-server-userapp crate
    // （routes.rs 测试模块）——本文档不再含 /api/userapp 路径。

    #[test]
    fn generated_document_round_trips_as_openapi() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let _unused: utoipa::openapi::OpenApi =
            serde_json::from_value(value).expect("deserialize OpenAPI");
    }

    #[test]
    fn json_and_multipart_request_bodies_are_typed() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let paths = value["paths"].as_object().expect("paths object");
        let schemas = value["components"]["schemas"]
            .as_object()
            .expect("component schemas");
        for (path, path_item) in paths {
            let Some(operation) = path_item.get("post") else {
                continue;
            };
            // 无请求体的 POST（cancel/stop 等动作接口）跳过；声明了 body 的必须类型化。
            let Some(content) = operation["requestBody"]["content"].as_object() else {
                continue;
            };
            let schema = content
                .values()
                .next()
                .and_then(|media| media.get("schema"))
                .unwrap_or_else(|| panic!("POST {path} must declare a request schema"));
            assert!(
                schema_has_named_fields(schema, schemas),
                "POST {path} request schema must expose named fields"
            );
        }
    }

    #[test]
    fn flattened_git_log_query_is_exposed_as_individual_parameters() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let parameters = value["paths"]["/api/git/log"]["get"]["parameters"]
            .as_array()
            .expect("Git log parameters");
        let names: Vec<&str> = parameters
            .iter()
            .filter_map(|parameter| parameter["name"].as_str())
            .collect();
        assert!(names.contains(&"workspaceType"));
        assert!(names.contains(&"maxCount"));
        assert!(!names.contains(&"base"));
    }

    #[test]
    fn file_operation_is_documented_as_enum() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let schemas = &value["components"]["schemas"];
        assert_eq!(
            schemas["FileOp"]["properties"]["operation"]["$ref"],
            "#/components/schemas/FileOperation"
        );
        assert_eq!(
            schemas["FileOperation"]["enum"],
            serde_json::json!(["create", "delete", "rename", "modify"])
        );
    }

    fn schema_has_named_fields(
        schema: &Value,
        components: &serde_json::Map<String, Value>,
    ) -> bool {
        if schema.get("properties").is_some() {
            return true;
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
            && let Some(name) = reference.strip_prefix("#/components/schemas/")
        {
            return components
                .get(name)
                .is_some_and(|component| schema_has_named_fields(component, components));
        }
        schema
            .get("allOf")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| schema_has_named_fields(part, components))
            })
    }
}

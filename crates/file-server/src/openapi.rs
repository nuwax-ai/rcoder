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
        (name = "Static", description = "Project and workspace static files"),
        (name = "UserApp", description = "UserApp workspace build and static")
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
        // 100 = 镜像族+build+dev 全量；+4 = app-files 转发族（rcoder /apps 文件面
        // 的容器侧实现）。路由增删须同步本计数（防"注册了但没进文档"回归）。
        assert_eq!(document.paths.paths.len(), 104);
        assert!(document.paths.paths.contains_key("/"));
        assert!(document.paths.paths.contains_key("/api/build/start-dev"));
        assert!(document.paths.paths.contains_key("/api/git/commit"));
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/app-files/upload")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/app-files/list")
        );
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
        assert!(document.paths.paths.contains_key("/api/userapp/build"));
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/projects/detect")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/projects/confirm")
        );
        // 文件操作镜像族（computer 域同参镜像, 15 个）
        for path in [
            "/api/userapp/get-file-list",
            "/api/userapp/resolve-file",
            "/api/userapp/search-files",
            "/api/userapp/files-update",
            "/api/userapp/upload-file",
            "/api/userapp/upload-files",
            "/api/userapp/generate-file",
            "/api/userapp/import-project",
            "/api/userapp/execute-command",
            "/api/userapp/get-logs",
            "/api/userapp/install-project",
            "/api/userapp/zip-workspace",
            "/api/userapp/download-all-files",
            "/api/userapp/init-project-template",
            "/api/userapp/push-skills-to-workspace",
        ] {
            assert!(
                document.paths.paths.contains_key(path),
                "userapp mirror path missing: {path}"
            );
        }
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/static/{app_id}")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/tasks/{task_id}")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/tasks/{task_id}/logs")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/tasks/{task_id}/logs/stream")
        );
        assert!(
            document
                .paths
                .paths
                .contains_key("/api/userapp/tasks/{task_id}/cancel")
        );
        assert!(document.paths.paths.keys().all(|path| !path.contains("{*")));
    }

    /// 文档质量防回归: /api/userapp/* 全部接口的 path/query 参数与请求体 schema 字段
    /// 必须有非空 description（doc comment 是唯一来源——新字段不写注释此处报红）。
    #[test]
    fn userapp_endpoints_fields_are_documented() {
        let document = generated_document();
        let mut checked_params = 0usize;
        let mut checked_fields = 0usize;
        for (path, item) in &document.paths.paths {
            if !path.starts_with("/api/userapp/") {
                continue;
            }
            for operation in [&item.get, &item.post].into_iter().flatten() {
                if let Some(params) = &operation.parameters {
                    for p in params {
                        assert!(
                            p.description.as_ref().is_some_and(|d| !d.trim().is_empty()),
                            "{path} 参数 {:?} 缺少 description（补 doc comment）",
                            p.name
                        );
                        checked_params += 1;
                    }
                }
            }
        }
        // 组件 schema 的字段 description（Userapp* Form/Body 与 dev server 的 DTO）
        for (name, schema) in &document
            .components
            .as_ref()
            .expect("components present")
            .schemas
        {
            if !name.starts_with("Userapp") && name != "DevOpBody" && name != "DevLogsQuery" {
                continue;
            }
            let utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(obj)) = schema
            else {
                continue;
            };
            for (field, value) in &obj.properties {
                let utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(field_obj)) =
                    value
                else {
                    continue;
                };
                assert!(
                    field_obj
                        .description
                        .as_ref()
                        .is_some_and(|d| !d.trim().is_empty()),
                    "schema {name} 字段 {field} 缺少 description（补 doc comment）"
                );
                checked_fields += 1;
            }
        }
        assert!(checked_params > 30, "参数检查覆盖异常: {checked_params}");
        assert!(
            checked_fields > 20,
            "schema 字段检查覆盖异常: {checked_fields}"
        );
    }

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

//! utoipa 注解式 OpenAPI 文档与内嵌 Swagger UI。
//!
//! 路径、参数和请求体由各 Axum handler 的 `#[utoipa::path]` 提供；
//! `utoipa_axum::OpenApiRouter` 在注册 handler 时同步收集文档，避免维护第二套路由表。
//! wire 契约类型（信封/Body/Query/响应载荷）集中在 [`crate::models`]。

use utoipa::{IntoResponses, OpenApi};
use utoipa_swagger_ui::{Config, SwaggerUi};

use crate::models::{ErrorResponse, SuccessResponse};

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
        // userapp 域 tag（file_server_document 会 merge file-server-userapp 域文档；
        // 未在此声明则 merge 后 UI 分组顺序退化为 operation 首现顺序。TS 老族与
        // app-files 在两份聚合文档均属内部剔除面，其 tag 只在容器侧独立文档声明）
        (name = "Userapp · dev · 构建任务", description = "dev 专属（目标容器恒为 UserappBuilder 开发容器）：构建触发、任务查询/取消与进度 SSE、制品包下载"),
        (name = "Userapp · dev · 工作区与工具链", description = "dev 专属（目标容器 UserappBuilder 开发容器）：workspace 创建、命令执行、打包下载、模板与技能安装、项目类型探测确认"),
        (name = "Userapp · dev · 进程管理", description = "dev 专属（目标容器 UserappBuilder 开发容器，路径自带 dev）：dev server 进程启停/列表/日志")
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
    use serde_json::Value;

    fn generated_document() -> utoipa::openapi::OpenApi {
        document(crate::routes::api_router().into_openapi())
    }

    /// 文档质量守卫：全部 components schema 的字段必须有非空 description
    /// （doc comment 是 swagger description 唯一来源——同事按 swagger 对接，
    /// 缺描述即对接盲区；与 file-server-userapp 的同款守卫对齐）。serde
    /// flatten 生成的 allOf 组合 schema（git 写操作族）逐 part 解引用后同
    /// 规则检查；$ref / array 取值的字段不带自身 description，跳过（与
    /// userapp 守卫同口径）。走序列化 JSON 而非 utoipa 类型 API（版本无关）。
    #[test]
    fn schema_fields_are_documented() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let schemas = value["components"]["schemas"]
            .as_object()
            .expect("component schemas");
        let mut checked = 0usize;
        let mut missing = Vec::new();

        fn check_props(
            label: &str,
            props: &serde_json::Map<String, Value>,
            checked: &mut usize,
            missing: &mut Vec<String>,
        ) {
            for (field, field_schema) in props {
                let Some(obj) = field_schema.as_object() else {
                    continue;
                };
                if !obj.contains_key("type") && !obj.contains_key("$ref") {
                    continue;
                }
                let documented = obj
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| !d.trim().is_empty());
                if documented {
                    *checked += 1;
                } else {
                    missing.push(format!("{label}.{field}"));
                }
            }
        }

        for (name, schema) in schemas {
            let Some(obj) = schema.as_object() else {
                continue;
            };
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                check_props(name, props, &mut checked, &mut missing);
            }
            if let Some(parts) = obj.get("allOf").and_then(Value::as_array) {
                for part in parts {
                    let target = part
                        .get("$ref")
                        .and_then(Value::as_str)
                        .and_then(|r| r.strip_prefix("#/components/schemas/"))
                        .and_then(|key| schemas.get(key))
                        .unwrap_or(part);
                    if let Some(props) = target.get("properties").and_then(Value::as_object) {
                        check_props(name, props, &mut checked, &mut missing);
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "schema 字段缺 description（补 doc comment）：\n{}",
            missing.join("\n")
        );
        assert!(checked > 250, "sanity: 覆盖异常, 实际 {checked}");
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
        // userApp 域已拆出：本文档不得再出现 /api/v1/userapp 路径（防回流）
        assert!(
            !document
                .paths
                .paths
                .keys()
                .any(|path| path.starts_with("/api/v1/userapp"))
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
    // （routes.rs 测试模块）——本文档不再含 /api/v1/userapp 路径。

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

    /// build/dev 族 200 响应必须类型化（防回退到通用 SuccessResponse 占位——
    /// 同事按 swagger 对接，业务字段不可见即盲区）。
    #[test]
    fn build_dev_endpoints_have_typed_success_bodies() {
        let value = serde_json::to_value(generated_document()).expect("serialize OpenAPI");
        let paths = value["paths"].as_object().expect("paths object");
        // (路径, 方法, 期望 schema 名)——dev/logs/build 族均为 GET（对齐 nuwax 路由形态）
        let expected = [
            ("/api/build/start-dev", "get", "DevStarted"),
            ("/api/build/stop-dev", "get", "DevStopped"),
            ("/api/build/restart-dev", "get", "DevStarted"),
            ("/api/build/list-dev", "get", "DevList"),
            ("/api/build/keep-alive", "get", "KeepAlive"),
            ("/api/build/port-pool-status", "get", "PortPool"),
            ("/api/build/get-dev-log", "get", "DevLog"),
            ("/api/build/get-log-cache-stats", "get", "LogCacheStats"),
            ("/api/build/clear-all-log-cache", "get", "Simple"),
            ("/api/build/parse-build-error", "post", "Simple"),
            ("/api/build/build", "get", "BuildDone"),
        ];
        for (path, method, schema_name) in expected {
            let operation = &paths[path][method];
            assert!(
                !operation.is_null(),
                "{method} {path} must be registered in the document"
            );
            let reference =
                &operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"];
            assert_eq!(
                reference,
                &serde_json::json!(format!("#/components/schemas/{schema_name}")),
                "{method} {path} 200 响应必须类型化为 {schema_name}（不得回退 SuccessResponse 占位）"
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

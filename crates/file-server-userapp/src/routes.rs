//! `/api/v1/userapp` 路由与文档（自 file-server routes/mod.rs 迁出）。

use axum::routing::options;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::UserAppState;
use crate::handlers::{
    static_files, userapp, userapp_app_files, userapp_dev, userapp_dev_server, userapp_files,
};

/// `/api/v1/userapp` 路由（workspace 多项目打包 + 文件操作镜像族 + 取整体包）。
fn userapp_router() -> OpenApiRouter<UserAppState> {
    OpenApiRouter::new()
        .routes(routes!(userapp::build_workspace))
        .routes(routes!(userapp::get_task))
        .routes(routes!(userapp::get_task_logs))
        .routes(routes!(userapp::stream_task_logs))
        .routes(routes!(userapp::cancel_task))
        .routes(routes!(userapp::detect_project))
        .routes(routes!(userapp::confirm_project))
        .routes(routes!(userapp_files::get_file_list))
        .routes(routes!(userapp_files::resolve_file))
        .routes(routes!(userapp_files::search_files))
        .routes(routes!(userapp_files::files_update))
        .routes(routes!(userapp_files::upload_file))
        .routes(routes!(userapp_files::upload_files))
        .routes(routes!(userapp_files::generate_file))
        .routes(routes!(userapp_files::import_project))
        .routes(routes!(userapp_app_files::upload))
        .routes(routes!(userapp_app_files::upload_from_url))
        .routes(routes!(userapp_app_files::list))
        .routes(routes!(userapp_app_files::delete))
        .routes(routes!(userapp_dev::ensure_workspace))
        .routes(routes!(userapp_dev::execute_command))
        .routes(routes!(userapp_dev::get_logs))
        .routes(routes!(userapp_dev::install_project))
        .routes(routes!(userapp_dev::zip_workspace))
        .routes(routes!(userapp_dev::download_all_files))
        .routes(routes!(userapp_dev::init_project_template))
        .routes(routes!(userapp_dev::push_skills_to_workspace))
        .routes(routes!(userapp_dev_server::dev_start))
        .routes(routes!(userapp_dev_server::dev_stop))
        .routes(routes!(userapp_dev_server::dev_restart))
        .routes(routes!(userapp_dev_server::dev_list))
        .routes(routes!(userapp_dev_server::dev_logs))
        .routes(routes!(static_files::serve_userapp))
        .route("/static/{app_id}", options(static_files::serve_userapp))
}

/// userApp 域顶层：nest `/api/v1/userapp` 前缀（路径注解为相对路径，文档收集时
/// 自动带前缀——与 file-server 侧原组织一致）。
pub(crate) fn userapp_top_router() -> OpenApiRouter<UserAppState> {
    OpenApiRouter::new().nest("/api/v1/userapp", userapp_router())
}

/// userApp 域独立 OpenAPI 文档（含 UserApp tag；rcoder 聚合与本地 swagger 用）。
#[derive(OpenApi)]
#[openapi(
    info(title = "file-server-userapp", description = "UserApp domain APIs (build/tasks/dev-server/file mirror)"),
    tags((name = "UserApp · 开发与构建", description = "开发工作区、构建任务与文件镜像（file-server 进程侧服务）"))
)]
struct ApiDoc;

/// 组装好的域文档：ApiDoc 基础 + 路由收集（路径带 `/api/v1/userapp` 前缀）。
pub fn document() -> utoipa::openapi::OpenApi {
    use utoipa::OpenApi as _;
    let mut doc = ApiDoc::openapi();
    doc.merge(userapp_top_router().into_openapi());
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径锚点 + 计数守卫（自 file-server openapi.rs 迁入）：路由全量注册且
    /// 路径规约（无 `{*}` 通配残留——OpenAPI path template 写作 `{rest}`）。
    #[test]
    fn document_contains_every_registered_operation() {
        let document = document();
        for path in [
            "/api/v1/userapp/build",
            "/api/v1/userapp/tasks/{task_id}",
            "/api/v1/userapp/tasks/{task_id}/logs",
            "/api/v1/userapp/tasks/{task_id}/logs/stream",
            "/api/v1/userapp/tasks/{task_id}/cancel",
            "/api/v1/userapp/projects/detect",
            "/api/v1/userapp/projects/confirm",
            "/api/v1/userapp/get-file-list",
            "/api/v1/userapp/resolve-file",
            "/api/v1/userapp/search-files",
            "/api/v1/userapp/files-update",
            "/api/v1/userapp/upload-file",
            "/api/v1/userapp/upload-files",
            "/api/v1/userapp/generate-file",
            "/api/v1/userapp/import-project",
            "/api/v1/userapp/app-files/upload",
            "/api/v1/userapp/app-files/upload-from-url",
            "/api/v1/userapp/app-files/list",
            "/api/v1/userapp/app-files/delete",
            "/api/v1/userapp/ensure-workspace",
            "/api/v1/userapp/execute-command",
            "/api/v1/userapp/get-logs",
            "/api/v1/userapp/install-project",
            "/api/v1/userapp/zip-workspace",
            "/api/v1/userapp/download-all-files",
            "/api/v1/userapp/init-project-template",
            "/api/v1/userapp/push-skills-to-workspace",
            "/api/v1/userapp/dev/start",
            "/api/v1/userapp/dev/stop",
            "/api/v1/userapp/dev/restart",
            "/api/v1/userapp/dev/list",
            "/api/v1/userapp/dev/logs",
            "/api/v1/userapp/static/{app_id}",
        ] {
            assert!(
                document.paths.paths.contains_key(path),
                "userapp path missing: {path}"
            );
        }
        assert_eq!(document.paths.paths.len(), 33);
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
        let document = document();
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
            checked >= 30,
            "sanity: 至少遍历 30 个 operation，实际 {checked}"
        );
    }

    /// 对接级描述锚点：SSE 事件契约与关键任务语义必须出现在 description 里——
    /// 同事按 swagger 直读对接，描述被精简回一句话会在此处报红。
    #[test]
    fn sse_and_task_endpoints_carry_event_contracts() {
        let document = document();
        let success_desc = |path: &str| -> String {
            let item = document
                .paths
                .paths
                .get(path)
                .unwrap_or_else(|| panic!("path {path} missing"));
            let op = item
                .get
                .as_ref()
                .or(item.post.as_ref())
                .unwrap_or_else(|| panic!("operation {path} missing"));
            let resp = op
                .responses
                .responses
                .get("200")
                .unwrap_or_else(|| panic!("{path} has no 200 response"));
            match resp {
                utoipa::openapi::RefOr::Ref(_) => panic!("{path} 200 response is a $ref"),
                utoipa::openapi::RefOr::T(resp) => resp.description.clone(),
            }
        };

        let sse = success_desc("/api/v1/userapp/tasks/{task_id}/logs/stream");
        for token in [
            "building",
            "build_ok",
            "completed",
            "failed",
            "cancelled",
            "stream_lagged",
            "Last-Event-ID",
            "fromSeq",
            "从 0 递增",
            "keep-alive",
            "artifactPath",
        ] {
            assert!(sse.contains(token), "SSE 描述缺事件/协议锚点 {token}");
        }

        // 续传头必须在参数清单里显式声明（同事按 swagger 对接，仅藏在描述里不可见）
        let sse_params = document
            .paths
            .paths
            .get("/api/v1/userapp/tasks/{task_id}/logs/stream")
            .and_then(|item| item.get.as_ref())
            .and_then(|op| op.parameters.as_ref())
            .map(|params| {
                params
                    .iter()
                    .map(|p| (p.name.as_str(), &p.parameter_in, &p.required))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let last_event_id = sse_params
            .iter()
            .find(|(name, r#in, _)| {
                *name == "Last-Event-ID" && matches!(r#in, utoipa::openapi::path::ParameterIn::Header)
            })
            .unwrap_or_else(|| panic!("SSE 接口参数缺 Last-Event-ID Header 声明（同事按 swagger 对接，头参数必须显式可见）"));
        assert!(
            matches!(last_event_id.2, utoipa::openapi::Required::False),
            "Last-Event-ID 是可选续传头, swagger 标必填会阻断不带头的首次订阅"
        );

        let build = success_desc("/api/v1/userapp/build");
        assert!(
            build.contains("taskId") && build.contains("artifactPath") && build.contains("pending"),
            "build 受理描述缺关键语义"
        );

        let task = success_desc("/api/v1/userapp/tasks/{task_id}");
        assert!(
            task.contains("completed")
                && task.contains("轮询")
                // 快照 seq 语义指引必须保留: 已发事件数=下一条 seq, 可直接作 fromSeq、
                // 勿作 Last-Event-ID(差 1, 直接用会漏事件)——同事按文档写续传逻辑
                && task.contains("fromSeq")
                && task.contains("勿直接作 Last-Event-ID"),
            "task 快照描述缺终态/轮询/seq 游标语义"
        );

        // static 取包: releaseId 可选参数（按版本取包）必须在文档参数清单里
        let static_op = document
            .paths
            .paths
            .get("/api/v1/userapp/static/{app_id}")
            .and_then(|item| item.get.as_ref())
            .expect("static path missing");
        assert!(
            static_op
                .parameters
                .as_ref()
                .is_some_and(|params| params.iter().any(|p| p.name == "releaseId"
                    && matches!(p.parameter_in, utoipa::openapi::path::ParameterIn::Query))),
            "static 接口参数缺 releaseId Query 声明（按版本取包是对外契约）"
        );
    }

    /// 全域兜底: `in=path` 只允许出现在路径模板同名占位符上。
    ///
    /// utoipa-axum 从 handler 签名自动发现 Query struct 时, 会按 Path extractor
    /// 推断 parameter_in——handler 同时有 Path 参数 + 裸名 Query struct 引用时,
    /// query 字段全部被误标 path（同事按 swagger 对接即错）。struct 上的
    /// `#[into_params(parameter_in = Query)]` 显式声明可覆盖; 本测试锁死
    /// 整个域不再出现任何误标（新增 Query struct 忘声明即报红）。
    #[test]
    fn query_params_never_mislabeled_as_path() {
        let document = document();
        for (path, item) in &document.paths.paths {
            for op in [&item.get, &item.post].into_iter().flatten() {
                let Some(params) = &op.parameters else {
                    continue;
                };
                for p in params {
                    if matches!(p.parameter_in, utoipa::openapi::path::ParameterIn::Path)
                        && !path.contains(&format!("{{{}}}", p.name))
                    {
                        panic!(
                            "参数 {name} 被标为 path 但路径模板无同名占位符 ({path}); \
                             Query struct 需 #[into_params(parameter_in = Query)] 显式声明",
                            name = p.name
                        );
                    }
                }
            }
        }
    }

    /// 文档质量防回归: 全部接口的 path/query 参数与请求体 schema 字段必须有
    /// 非空 description（doc comment 是唯一来源——新字段不写注释此处报红）。
    /// 自 file-server openapi.rs 迁入。
    #[test]
    fn userapp_endpoints_fields_are_documented() {
        let document = document();
        let mut checked_params = 0usize;
        let mut checked_fields = 0usize;
        for (path, item) in &document.paths.paths {
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

    /// 新契约接口响应必须包 HttpResult 信封（`{code, message, data, tid, success}`），
    /// 防响应形态漂移回裸 JSON / TS 风格。TS 迁移镜像族（userapp_files/app_files、
    /// execute-command 等 15 个）豁免——保持 nuwax-file-server 原形态。
    /// 自 file-server openapi.rs 迁入。
    #[test]
    fn userapp_new_contract_endpoints_are_http_result_enveloped() {
        let value = serde_json::to_value(document()).expect("serialize OpenAPI");
        let paths = value["paths"].as_object().expect("paths object");
        // 6 个 Rust 新契约接口（TS 无对应端点）：dev 生命周期 5 + ensure-workspace
        let expected_refs = [
            (
                "/api/v1/userapp/dev/start",
                "HttpResult_UserappDevTaskCreated",
            ),
            ("/api/v1/userapp/dev/stop", "HttpResult_UserappDevStopped"),
            (
                "/api/v1/userapp/dev/restart",
                "HttpResult_UserappDevTaskCreated",
            ),
            ("/api/v1/userapp/dev/list", "HttpResult_UserappDevList"),
            ("/api/v1/userapp/dev/logs", "HttpResult_ReadDevLogResult"),
            (
                "/api/v1/userapp/ensure-workspace",
                "HttpResult_UserappEnsureWorkspaceData",
            ),
        ];
        for (path, expected_schema) in expected_refs {
            let operation = paths[path]
                .get("post")
                .or_else(|| paths[path].get("get"))
                .unwrap_or_else(|| panic!("{path} must be registered"));
            let schema = &operation["responses"]["200"]["content"]["application/json"]["schema"];
            let reference = schema["$ref"].as_str().unwrap_or_else(|| {
                panic!("{path} 200 response must $ref a named schema, got: {schema}")
            });
            assert_eq!(
                reference,
                format!("#/components/schemas/{expected_schema}"),
                "{path} 响应必须包 HttpResult 信封（data 载荷 = {expected_schema}）"
            );
        }
    }
}

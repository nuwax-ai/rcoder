//! OpenAPI 聚合文档守卫测试（从 router_docs.rs 内联模块拆出）。

use super::*;
use axum::Router;

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

fn assert_summaries_ui_concise(doc_name: &str, document: &utoipa::openapi::OpenApi) {
    const METHODS: [&str; 8] = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD", "TRACE",
    ];
    let mut checked = 0usize;
    for (path, item) in &document.paths.paths {
        for (method, op) in operations_of(item) {
            let Some(summary) = op.summary.as_deref().filter(|s| !s.trim().is_empty()) else {
                panic!(
                    "{doc_name} {method} {path}: summary 缺失（doc comment 首段或显式 summary= 必填）"
                );
            };
            assert!(
                !summary.contains('\n'),
                "{doc_name} {method} {path}: summary 须为单行（多行内容移到空行后的详细段）"
            );
            assert!(
                summary.chars().count() <= 50,
                "{doc_name} {method} {path}: summary 过长（>50 字符），详细内容移入 description: {summary}"
            );
            let method_prefixed =
                summary.starts_with('`') && METHODS.iter().any(|m| summary[1..].starts_with(m));
            assert!(
                !method_prefixed,
                "{doc_name} {method} {path}: summary 不得带方法/路径前缀（UI 已单独显示）: {summary}"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 60,
        "{doc_name}: sanity 下限未达（至少遍历 60 个 operation，实际 {checked}）"
    );
}

/// 两份对外文档的 operation summary 适配文档 UI（Scalar 左侧菜单与详情区
/// 标题显示它）：非空、单行、≤50 字符、无方法/路径前缀。utoipa 取 doc
/// comment 首段为 summary——首段写长文/带 `` `POST /api/...`` 前缀在此报红。
#[test]
fn operation_summaries_are_ui_concise() {
    assert_summaries_ui_concise("primary", &primary_document());
    assert_summaries_ui_concise("file-server", &file_server_document());
}

fn sole_tag(
    doc_name: &str,
    document: &utoipa::openapi::OpenApi,
    path: &str,
    method: &str,
) -> String {
    let item = document
        .paths
        .paths
        .get(path)
        .unwrap_or_else(|| panic!("{doc_name}: path {path} 不在文档中"));
    let (_, op) = operations_of(item)
        .into_iter()
        .find(|(m, _)| *m == method)
        .unwrap_or_else(|| panic!("{doc_name}: {path} 无 {method} operation"));
    let mut tags = op.tags.clone().unwrap_or_default();
    assert_eq!(
        tags.len(),
        1,
        "{doc_name} {method} {path}: 期望单 tag，实际 {tags:?}"
    );
    tags.remove(0)
}

/// 主文档按环境维度分组：UserApp 十三个子类 tag（dev 专属 → prod 专属 →
/// 双态 → 访问入口）声明齐全且顺序最前（UI 分组顺序 = tags 声明顺序）、
/// legacy tag（应用管理/应用日志/Computer Agent）全文清零、UserApp 系
/// operation 计数下限、分域锚点逐一断言。
#[test]
fn primary_document_groups_userapp_by_business_domain() {
    let document = primary_document();
    let tag_names: Vec<&str> = document
        .tags
        .iter()
        .flatten()
        .map(|t| t.name.as_str())
        .collect();
    let userapp_tags = [
        "UserApp · dev · 构建任务",
        "UserApp · dev · 工作区与工具链",
        "UserApp · dev · 进程管理",
        "UserApp · dev · 终端工具",
        "UserApp · prod · 部署与启停",
        "UserApp · prod · 应用查询",
        "UserApp · prod · 终端工具",
        "UserApp · 双态 · 文件与存储",
        "UserApp · 双态 · 日志",
        "UserApp · 双态 · 数据库",
        "UserApp · 双态 · 生命周期",
        "UserApp · 访问入口",
    ];
    let mut prev: Option<usize> = None;
    for tag in userapp_tags {
        let pos = tag_names
            .iter()
            .position(|n| *n == tag)
            .unwrap_or_else(|| panic!("tags 声明缺失: {tag}"));
        if let Some(prev_pos) = prev {
            assert!(
                pos > prev_pos,
                "tags 声明顺序异常: {tag}（pos={pos} prev={prev_pos}）"
            );
        }
        prev = Some(pos);
    }
    assert!(
        tag_names.first().is_some_and(|n| n.starts_with("UserApp")),
        "UserApp 子类必须排在 tags 声明最前"
    );
    // 环境维度重组前的旧功能面 tag 视同 legacy：声明与 operation 双清零
    let legacy_tags = [
        "应用管理",
        "应用日志",
        "Computer Agent",
        "UserApp · 生命周期",
        "UserApp · 日志",
        "UserApp · 文件与存储",
        "UserApp · 数据库",
        "UserApp · 终端与代理",
        "UserApp · 开发与构建",
        "UserApp · 生产运维",
    ];
    for legacy in legacy_tags {
        assert!(
            !tag_names.contains(&legacy),
            "legacy tag 声明残留: {legacy}"
        );
    }

    let mut userapp_ops = 0usize;
    for (path, item) in &document.paths.paths {
        for (method, op) in operations_of(item) {
            for tag in op.tags.iter().flatten() {
                let tag = tag.as_str();
                assert!(
                    !legacy_tags.contains(&tag),
                    "{method} {path}: legacy tag 残留: {tag}"
                );
                if tag.starts_with("UserApp") {
                    userapp_ops += 1;
                }
            }
        }
    }
    // 口径（环境维度 tag 重组后同前）：内部剔除 21 条后实测 UserApp 系 op
    // 总数=52（dev/logs、tasks/{id}/logs 先后下线各 -1），下限防漂移不设满额
    assert!(
        userapp_ops >= 52,
        "UserApp 系 operation 计数下限（52）未达: {userapp_ops}"
    );

    let tag_of = |path: &str, method: &str| sole_tag("primary", &document, path, method);
    // 环境维度锚点：dev 专属 / prod 专属 / 双态 / 访问入口 各至少一条
    assert_eq!(
        tag_of("/api/v1/userapp/build", "post"),
        "UserApp · dev · 构建任务"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/static/{app_id}", "get"),
        "UserApp · dev · 构建任务"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/dev/stop", "post"),
        "UserApp · dev · 进程管理"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/{app_id}/start", "post"),
        "UserApp · prod · 部署与启停"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/{app_id}/stop", "post"),
        "UserApp · prod · 部署与启停"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/{app_id}/{app_stage}/storage", "get"),
        "UserApp · 双态 · 文件与存储"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/{app_id}/{app_stage}/logs/stream", "post"),
        "UserApp · 双态 · 日志"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/db/{app_stage}/reset-password", "post"),
        "UserApp · 双态 · 数据库"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/db/{app_stage}/align-credentials", "post"),
        "UserApp · 双态 · 数据库"
    );
    assert_eq!(
        tag_of("/api/v1/userapp/{app_id}/{app_stage}/health", "get"),
        "UserApp · 双态 · 生命周期"
    );
    assert_eq!(
        tag_of("/proxy/userapp/dev/{user_id}/{app_id}/{*path}", "get"),
        "UserApp · 访问入口"
    );
    assert_eq!(tag_of("/userapp/routes", "get"), "UserApp · 访问入口");
    assert_eq!(
        tag_of("/computer/db/{user_id}/reset-password", "post"),
        "computer"
    );
}

/// userApp 全接口 app_id + user_id 文档可见性防回归（用户铁律：
/// 所有 userApp 业务接口都要有 app_id 和 user_id 入参，方便获取使用）。
///
/// 遍历主文档全部 /api/v1/userapp op，断言两字段在 path 模板 / query 参数 /
/// request body schema 属性任一处可见。豁免清单显式枚举（列表跨 app 查询
/// 天然无单 app 归属——user_id 仍必须）：
/// - `query` / `runtime` / `storage/{app_stage}/query` 三条的 app_id
#[test]
fn userapp_params_app_and_owner_visible() {
    let document = primary_document();
    let schemas = &document.components.as_ref().map(|c| c.schemas.clone());
    /// 豁免：列表跨 app 查询类（无单 app 归属，user_id 仍必须）
    const APP_ID_EXEMPT: [&str; 3] = [
        "/api/v1/userapp/query",
        "/api/v1/userapp/runtime",
        "/api/v1/userapp/storage/{app_stage}/query",
    ];
    let mut checked = 0usize;
    for (path, item) in &document.paths.paths {
        if !path.starts_with("/api/v1/userapp") {
            continue;
        }
        for (method, op) in operations_of(item) {
            let params = op.parameters.iter().flatten();
            let mut has_app_id =
                APP_ID_EXEMPT.contains(&path.as_str()) || path.contains("{app_id}");
            let mut has_user_id = false;
            for p in params {
                // 15 族镜像接口的 camelCase（appId/userId）是永久 TS 契约——
                // 参数名双词表兼容（容器侧 IntoParams serde rename_all camelCase）
                if p.name == "app_id" || p.name == "appId" {
                    has_app_id = true;
                }
                if p.name == "user_id" || p.name == "userId" {
                    has_user_id = true;
                }
            }
            // request body schema 属性兜底（$ref 解引用）
            if !has_app_id || !has_user_id {
                let body_ref = op
                    .request_body
                    .as_ref()
                    .and_then(|rb| rb.content.values().next())
                    .and_then(|c| c.schema.as_ref())
                    .and_then(|s| match s {
                        utoipa::openapi::RefOr::Ref(r) => Some(r.clone()),
                        _ => None,
                    });
                if let Some(r) = body_ref
                    && let Some(schemas) = schemas
                    && let Some(utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(
                        obj,
                    ))) = schemas.get(&r.ref_location.replace("#/components/schemas/", ""))
                {
                    for k in obj.properties.keys() {
                        if k == "app_id" || k == "appId" {
                            has_app_id = true;
                        }
                        if k == "user_id" || k == "userId" {
                            has_user_id = true;
                        }
                    }
                }
            }
            assert!(
                has_app_id,
                "{method} {path}: 缺 app_id 入参（path/query/body 任一位置）——用户铁律：userApp 接口必须携带"
            );
            assert!(
                has_user_id,
                "{method} {path}: 缺 user_id 入参（query/body 任一位置）——用户铁律：userApp 接口必须携带"
            );
            checked += 1;
        }
    }
    // 本守卫口径 = 主文档 /api/v1/userapp 前缀 op（tag 口径 52 含部分内部面差异，
    // 此处用本面实测全量 41 作下限——接口只增不减；dev/logs、tasks/{id}/logs 先后下线）
    assert!(
        checked >= 41,
        "UserApp 系 operation 计数下限（41）未达: {checked}"
    );
}

/// 环境维度防回归：UserApp 系 operation 的 tag 必须带环境段（dev/prod/双态
/// 三选一）或属于「访问入口」——今后新增接口随手写无环境维度的 tag 即红，
/// 保证 Scalar 分组永远可按 dev 专属 / prod 专属 / 双态辨识。
#[test]
fn userapp_tags_carry_environment_dimension() {
    let document = primary_document();
    let mut userapp_ops = 0usize;
    for (path, item) in &document.paths.paths {
        for (method, op) in operations_of(item) {
            for tag in op.tags.iter().flatten() {
                let tag = tag.as_str();
                if !tag.starts_with("UserApp") {
                    continue;
                }
                userapp_ops += 1;
                let well_formed = tag.starts_with("UserApp · dev · ")
                    || tag.starts_with("UserApp · prod · ")
                    || tag.starts_with("UserApp · 双态 · ")
                    || tag == "UserApp · 访问入口";
                assert!(
                    well_formed,
                    "{method} {path}: UserApp tag 缺环境维度（须为 \
                     `UserApp · dev|prod|双态 · 功能` 或 `UserApp · 访问入口`）: {tag}"
                );
            }
        }
    }
    assert!(
        userapp_ops >= 52,
        "UserApp 系 operation 计数下限（52）未达: {userapp_ops}"
    );
    // 三个环境组都必须非空：分组退化（全塞一组/环境段丢失）当场报红
    for prefix in ["UserApp · dev · ", "UserApp · prod · ", "UserApp · 双态 · "] {
        assert!(
            document
                .tags
                .iter()
                .flatten()
                .any(|t| t.name.starts_with(prefix)),
            "tags 声明缺失环境组: {prefix}"
        );
    }
    // 声明的 UserApp tag 必有 operation 引用：merge 进来的容器域声明遇
    // 整族内部剔除会变空组（UI 空区块），prune_empty_tags 已剪、此处锁死
    let declared: Vec<&str> = document
        .tags
        .iter()
        .flatten()
        .map(|t| t.name.as_str())
        .filter(|n| n.starts_with("UserApp"))
        .collect();
    let used: std::collections::HashSet<&str> = document
        .paths
        .paths
        .values()
        .flat_map(operations_of)
        .flat_map(|(_, op)| op.tags.iter().flatten().map(String::as_str))
        .filter(|t| t.starts_with("UserApp"))
        .collect();
    for tag in declared {
        assert!(
            used.contains(tag),
            "声明了但零 operation 的空 UserApp tag: {tag}"
        );
    }
}

/// 运行时承接面与文档面双向闭包：主文档可见的每个 userapp path 必须被
/// 三张登记表（本地实现 / app_manager / 容器透传）覆盖，透传清单每条必居
/// 「主文档可见」或「INTERNAL 内部剔除」两态——接口上下线忘同步登记表
/// 当场报红（此前透传靠 catch-all 兜底，此类漂移不可见）。
#[test]
fn primary_userapp_paths_are_fully_handled_by_route_tables() {
    use crate::userapp_forward::CONTAINER_PASS_THROUGH_PATHS;
    use crate::userapp_forward::guard_tables::{APP_MANAGER_PATHS, LOCAL_USERAPP_PATHS};
    let document = primary_document();
    for path in document.paths.paths.keys() {
        if !path.starts_with("/api/v1/userapp/") {
            continue;
        }
        assert!(
            LOCAL_USERAPP_PATHS.contains(&path.as_str())
                || APP_MANAGER_PATHS.contains(&path.as_str())
                || CONTAINER_PASS_THROUGH_PATHS.contains(&path.as_str()),
            "primary doc userapp path has no runtime route: {path}"
        );
    }
    for path in CONTAINER_PASS_THROUGH_PATHS.iter().copied() {
        assert!(
            document.paths.paths.contains_key(path) || INTERNAL_USERAPP_PATHS.contains(&path),
            "pass-through path neither in primary doc nor internal-stripped: {path}"
        );
    }
}

/// 运行日志 SSE 契约锚点：事件清单必须出现在 description 里（同事按 swagger
/// 直读对接，描述被精简回一句话在此报红——对齐 file-server-userapp 同款测试）。
#[test]
fn app_logs_stream_description_carries_sse_contract() {
    let document = ApiDoc::openapi();
    let item = document
        .paths
        .paths
        .get("/api/v1/userapp/{app_id}/{app_stage}/logs/stream")
        .expect("logs/stream path documented");
    let op = item.post.as_ref().expect("POST operation");
    let resp = op
        .responses
        .responses
        .get("200")
        .expect("200 response present");
    let desc = match resp {
        utoipa::openapi::RefOr::Ref(_) => panic!("200 response is a $ref"),
        utoipa::openapi::RefOr::T(r) => r.description.clone(),
    };
    for token in [
        "log",
        "source_error",
        "source_recovered",
        "cursor_reset",
        "checkpoint",
        "heartbeat",
        "cursor",
    ] {
        assert!(
            desc.contains(token),
            "logs/stream 描述缺 SSE 事件锚点 {token}"
        );
    }
}

#[test]
fn userapp_release_log_and_publish_paths_are_documented() {
    let document = ApiDoc::openapi();
    let paths = document.paths.paths;
    for path in [
        // releases 五接口已随 RBD 卷形态删除（部署只走 start+url，见 handbook 10）
        "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
        "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
        "/api/v1/userapp/{app_id}/start",
    ] {
        assert!(paths.contains_key(path), "OpenAPI path missing: {path}");
    }
    // 删除面防复活：releases + rcoder 侧 publish 任务体系路径不得再出现
    // （构建链收敛为 file-server /api/v1/userapp/* 接口族，rcoder 不再做发布编排）
    for gone in [
        "/api/v1/userapp/{app_id}/releases/prepare",
        "/api/v1/userapp/{app_id}/releases/rollback",
        "/api/v1/userapp/{app_id}/releases/{release_id}/activate",
        "/api/v1/userapp/{app_id}/build",
        "/api/v1/userapp/publish/tasks/query",
        "/api/v1/userapp/publish/tasks/{task_id}",
        "/api/v1/userapp/publish/tasks/{task_id}/stream",
        "/api/v1/userapp/publish/tasks/{task_id}/cancel",
    ] {
        assert!(!paths.contains_key(gone), "deleted path reappeared: {gone}");
    }
}

/// file-server 文档聚合进 rcoder Swagger UI（全量剔除
/// [`INTERNAL_USERAPP_PATHS`] 内部路由面后挂载）。此测试锁定聚合链路活着:
/// 语义锚点 + 动态下限——逐条路径清单由 file-server 自己的 openapi 测试
/// （总数 + contains_key）锁定, 这里不重复维护。
#[test]
fn file_server_document_covers_userapp_and_project_paths() {
    let document = file_server_document();
    let paths = &document.paths.paths;
    // 锚点: 项目创建入口 + UserApp 打包链 (跨域语义关键路径)
    for path in [
        "/api/project/create-project",
        "/api/v1/userapp/build",
        "/api/v1/userapp/dev/start",
    ] {
        assert!(
            paths.contains_key(path),
            "file-server OpenAPI path missing: {path}"
        );
    }
    let userapp_count = paths
        .keys()
        .filter(|p| p.starts_with("/api/v1/userapp/"))
        .count();
    // 内部剔除扩容（13→21：新增 app-files 族五条 + 构建链 dev-only 三条上收）
    // 后实测 userapp 面路径数下降；下限防漂移不设满额
    assert!(paths.len() >= 85, "聚合文档路径总数异常: {}", paths.len());
    assert!(userapp_count >= 14, "userapp 路径数异常: {userapp_count}");
}

/// 内部路由面防回归: 13 个 [`INTERNAL_USERAPP_PATHS`]（file-server-proxy 分流
/// 代理的上游镜像接口）不得出现在任何一份对外文档；同时保留面锚点仍在
/// （userApp 公开域: dev 生命周期/编译/打包/任务/静态）。
#[test]
fn internal_userapp_paths_are_hidden_from_docs() {
    let primary = primary_document();
    for path in INTERNAL_USERAPP_PATHS {
        assert!(
            !primary.paths.paths.contains_key(path),
            "主文档泄露内部路由: {path}"
        );
    }
    let file_server_doc = file_server_document();
    for path in INTERNAL_USERAPP_PATHS {
        assert!(
            !file_server_doc.paths.paths.contains_key(path),
            "file-server 文档泄露内部路由: {path}"
        );
    }
    // 保留面锚点（不在内部清单的 userApp 公开接口）
    for anchor in [
        "/api/v1/userapp/build",
        "/api/v1/userapp/dev/start",
        "/api/v1/userapp/get-logs",
        "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
    ] {
        assert!(
            primary.paths.paths.contains_key(anchor),
            "主文档缺 userApp 公开路径: {anchor}"
        );
    }
}

/// HTTP 层验证：两份 openapi.json 均由 Swagger UI 路由实际提供服务。
/// 主文档额外验证 userApp 域已合入（默认打开即可见）。
#[tokio::test]
async fn swagger_ui_serves_both_documents() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let app = Router::new().merge(create_swagger_ui());
    for (path, needle) in [
        ("/api/docs/openapi.json", "/api/v1/userapp"),
        ("/api/docs/openapi.json", "/api/v1/userapp/dev/start"),
        ("/api/docs/file-server.json", "/api/v1/userapp/build"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "GET {path} 非 200");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(needle), "{path} 响应缺少 {needle}");
    }
}

/// Scalar 文档页 HTTP 层验证：两个页面均实际提供服务，spec 内嵌于
/// HTML（含各文档语义锚点路径）。与 SwaggerUi 同 Router merge 构造
/// 即验证 `/api/docs/{*rest}` 通配与 `/api/docs/scalar` static 共存
/// 不冲突（冲突会在 merge 时 panic）。
#[tokio::test]
async fn scalar_docs_serve_both_documents() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let app = Router::new()
        .merge(create_swagger_ui())
        .merge(create_scalar_docs());
    for (path, needle) in [
        // 主文档页：Scalar 引导脚本 + spec 锚点（应用管理 + userApp）
        ("/api/docs/scalar", "@scalar/api-reference"),
        ("/api/docs/scalar", "/api/v1/userapp"),
        ("/api/docs/scalar", "/api/v1/userapp/dev/start"),
        // file-server 页：全量文档锚点
        ("/api/docs/scalar/file-server", "/api/v1/userapp/build"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "GET {path} 非 200");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(needle), "{path} 响应缺少 {needle}");
    }
    // Swagger UI 原有路由不受 Scalar 挂载影响（static 优先不改变通配兜底）
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/docs/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET /api/docs/ 非 200");
}

/// 主文档选择性合入的语义锁定：userApp 域（/api/v1/userapp/*）在、其余
/// file-server 域（project 等）不在（防主文档膨胀）。
#[test]
fn primary_document_merges_userapp_domain_only() {
    let paths = &primary_document().paths.paths;
    for anchor in ["/api/v1/userapp/dev/start", "/api/v1/userapp/build"] {
        assert!(
            paths.contains_key(anchor),
            "主文档缺 userApp 路径: {anchor}"
        );
    }
    assert!(
        !paths.contains_key("/api/project/create-project"),
        "project 域不应合入主文档（留在 file-server.json）"
    );
}

/// UserApp 全部对接端点（`/api/v1/userapp` + `/userapp/` 代理文档接口）的文档质量
/// 防回归：
/// 1. 每个操作必须有非空 summary 或 description（handler `///` doc 注释）；
/// 2. 成功响应（2xx/3xx——/userapp/ 文档接口的成功码是 307）必须有非空 description；
/// 3. 必须声明至少一个 4xx/5xx 错误响应（与 handler 实际错误分支对应）。
///
/// 新增 UserApp 端点未写注释会在此失败——样板见 app_manager/handlers（/api/v1/userapp 族）。
#[test]
fn userapp_openapi_annotations_are_complete() {
    let document = ApiDoc::openapi();
    let mut checked = 0usize;
    for (path, item) in &document.paths.paths {
        // /userapp/routes 速查表是纯静态文档接口（无错误分支），不在质量检查内
        let is_userapp_proxy_doc = ["/userapp/dev/", "/userapp/prod/"]
            .iter()
            .any(|prefix| path.starts_with(prefix));
        if !path.starts_with("/api/v1/userapp") && !is_userapp_proxy_doc {
            continue;
        }
        for operation in [&item.get, &item.post].into_iter().flatten() {
            let described = operation
                .summary
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty())
                || operation
                    .description
                    .as_ref()
                    .is_some_and(|d| !d.trim().is_empty());
            assert!(
                described,
                "OpenAPI 操作缺少 doc 注释（summary/description 均为空）: {path}"
            );

            let responses = &operation.responses.responses;
            let success = responses.keys().find_map(|code| {
                let status = code.trim().parse::<u16>().ok()?;
                (200..400).contains(&status).then_some(code.clone())
            });
            let success =
                success.unwrap_or_else(|| panic!("OpenAPI 操作缺少 2xx/3xx 成功响应: {path}"));
            let utoipa::openapi::RefOr::T(ok) = responses
                .get(&success)
                .unwrap_or_else(|| panic!("OpenAPI 操作缺少 {success} 响应: {path}"))
            else {
                panic!("{success} 响应不应为 $ref: {path}")
            };
            assert!(
                !ok.description.trim().is_empty(),
                "{success} 响应缺少 description: {path}"
            );

            let has_error_code = responses.keys().any(|code| {
                code.trim()
                    .parse::<u16>()
                    .is_ok_and(|c| (400..600).contains(&c))
            });
            assert!(
                has_error_code,
                "OpenAPI 操作未声明任何 4xx/5xx 错误响应: {path}"
            );
            checked += 1;
        }
    }
    // 覆盖数下限：app_manager 25（create REST 面 + releases 五接口已删）+
    // app_manager /api/v1/userapp 族 + /userapp/ 代理文档 6（开发域 ttyd/vnc/audio/ime
    // + 运行容器 ttyd/pgweb）+ dbx 两阶段代理文档 2（dev/prod）。
    assert!(
        checked >= 33,
        "UserApp OpenAPI 端点覆盖数异常偏少: {checked}"
    );
}

/// 文档质量防回归：`/computer/pod/*` 接口的 GET 参数与 POST 请求体 DTO 字段
/// 必须有非空 description（doc comment 是 swagger description 唯一来源，
/// 同事看 swagger 对接；pod 域此前零字段级覆盖，userApp 三字段形态
/// service_type=userapp/app_id/app_stage 靠此测试守卫）。
#[test]
fn pod_endpoints_fields_are_documented() {
    let document = ApiDoc::openapi();
    let mut checked_params = 0usize;
    let mut checked_fields = 0usize;
    for (path, item) in &document.paths.paths {
        if !path.starts_with("/computer/pod/") {
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
    // 请求体 DTO（POST 三兄弟）的 schema 字段（$ref 字段如 resource_limits 跳过）
    for (name, schema) in &document
        .components
        .as_ref()
        .expect("components present")
        .schemas
    {
        if !matches!(
            name.as_str(),
            "EnsurePodRequest" | "KeepalivePodRequest" | "RestartPodRequest"
        ) {
            continue;
        }
        let utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(obj)) = schema else {
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
    // 动态下限防空转：GET status/vnc-status 各 9 参数（含新增 app_id/app_stage）；
    // 三个 POST DTO 各 9 个内联字段（resource_limits 为 $ref 不计）。
    assert!(
        checked_params >= 16,
        "pod 接口参数覆盖数异常偏少: {checked_params}"
    );
    assert!(
        checked_fields >= 24,
        "pod 请求体字段覆盖数异常偏少: {checked_fields}"
    );
}

//! OpenAPI 文档聚合与文档 UI（从 router.rs 拆出；路由组装仍在 router.rs）。
//!
//! [`ApiDoc`] 是 rcoder 主文档的 utoipa 声明（paths/components 全量，
//! 声明体在 [`api_doc`]）；[`create_swagger_ui`] 聚合两份文档（主文档 +
//! file-server 全量文档，UI 顶部下拉切换），[`create_scalar_docs`] 以
//! Scalar 界面提供同样两份文档（每份独立页面，供对比选用）。

mod api_doc;
#[cfg(test)]
mod openapi_tests;

pub use api_doc::ApiDoc;

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// 创建 Swagger UI 路由。
///
/// 内部路由 path（file-server-proxy 分流代理的上游镜像接口）：路由保留、对外
/// 文档不暴露——Java 同事调 computer 域同名接口，带 `x-service-type: userapp`
/// header 经 60000 分流代理内部路由到这些；直接暴露会让调用方绕过分流契约。
const INTERNAL_USERAPP_PATHS: [&str; 21] = [
    "/api/v1/userapp/download-all-files",
    "/api/v1/userapp/files-update",
    "/api/v1/userapp/generate-file",
    "/api/v1/userapp/get-file-list",
    "/api/v1/userapp/import-project",
    "/api/v1/userapp/execute-command",
    "/api/v1/userapp/push-skills-to-workspace",
    "/api/v1/userapp/resolve-file",
    "/api/v1/userapp/search-files",
    "/api/v1/userapp/upload-file",
    "/api/v1/userapp/upload-files",
    "/api/v1/userapp/workspace",
    "/api/v1/userapp/zip-workspace",
    // app-files 族五条：dev storage/clear 与文件族 ({app_id}/{app_stage}/upload、
    // files、files/delete) 的容器侧实现端点（rcoder 出站调用），对外语义由
    // 相应 `{app_stage}` 门面承载
    "/api/v1/userapp/app-files/clear",
    "/api/v1/userapp/app-files/upload",
    "/api/v1/userapp/app-files/upload-from-url",
    "/api/v1/userapp/app-files/list",
    "/api/v1/userapp/app-files/delete",
    // 构建链 dev-only 三条：已上收 `{app_id}/{app_stage}` 门面路由（folded URI 转发，
    // 容器侧平铺端点保留），平铺形态不再对外暴露
    "/api/v1/userapp/projects/detect",
    "/api/v1/userapp/projects/confirm",
    "/api/v1/userapp/install-project",
];

/// 从文档剔除内部路由 path（components 中失去引用的 schema 残留无害，不追引
/// 用图清理）。
fn strip_internal_userapp_paths(document: &mut utoipa::openapi::OpenApi) {
    document
        .paths
        .paths
        .retain(|path, _| !INTERNAL_USERAPP_PATHS.contains(&path.as_str()));
}

/// 聚合两份文档（UI 顶部下拉切换）：rcoder 主文档 + file-server 文档。
/// file-server 全量文档（含 /api/v1/userapp）始终聚合在此；实际路由宿主：
/// 老路径（project/computer/git/build）常驻 rcoder 主服务（`merged_router`），
/// userapp 域由 rcoder 转发层接管、本地实现在 per-app 开发容器内 file-server（60000）。
///
/// 主文档额外合入 userApp 业务域（file-server 的 `/api/v1/userapp/*` 路径 +
/// schemas）——Swagger 默认打开主文档即见 userApp 全貌（dev 生命周期/编译/
/// 文件/静态），无需切下拉；其余 file-server 域（project/computer/git 等）
/// 仍只在 file-server.json，防主文档膨胀。两份文档均剔除
/// `INTERNAL_USERAPP_PATHS`（分流代理的内部路由面）。
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", primary_document())
        .url("/api/docs/file-server.json", file_server_document())
        .config(utoipa_swagger_ui::Config::new([
            "/api/docs/openapi.json",
            "/api/docs/file-server.json",
        ]))
}

/// Scalar 风格文档 UI（与 Swagger UI 并存试用，文档构造完全复用）。
///
/// 一实例一文档（Scalar 无多文档下拉），两份文档挂两个页面：
/// - `/api/docs/scalar`：主文档（应用管理 + userApp 业务域）
/// - `/api/docs/scalar/file-server`：file-server 全量文档
///
/// 注意：Scalar 的 UI JS 由页面从公网 CDN（jsdelivr）加载，spec 本身
/// 内嵌在返回的 HTML 中——浏览器无法出网时页面会白屏（Swagger UI 资产
/// 是编译期内嵌的，不受影响）；届时用 `custom_html` 换自托管 JS。
/// 路由共存依赖 matchit static 优先于 `/api/docs/{*rest}` 通配。
pub fn create_scalar_docs() -> axum::Router {
    use utoipa_scalar::{Scalar, Servable};
    // `.title()` 在已发布的 0.3.0 尚未提供（master 未发版），页面标题
    // 用默认 "Scalar"；升级 0.4+ 可补。
    axum::Router::new()
        .merge(Scalar::with_url("/api/docs/scalar", primary_document()))
        .merge(Scalar::with_url(
            "/api/docs/scalar/file-server",
            file_server_document(),
        ))
}

/// file-server 下拉文档 = file-server 全量文档（TS 对齐域）+ userApp 域文档
/// （file-server-userapp crate 独立产出）剔除内部路由 path。
fn file_server_document() -> utoipa::openapi::OpenApi {
    let mut doc = file_server::openapi::document(file_server::routes::api_router().into_openapi());
    doc.merge(file_server_userapp::document());
    strip_internal_userapp_paths(&mut doc);
    prune_empty_tags(&mut doc);
    doc
}

/// 主文档 = rcoder 应用管理 + userApp 业务域（选择性合入）。
fn primary_document() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    let mut userapp = file_server_userapp::document();
    strip_internal_userapp_paths(&mut userapp);
    doc.merge(userapp);
    prune_empty_tags(&mut doc);
    doc
}

/// 剪掉「已声明但无 operation 引用」的 tag：merge 进来的容器域文档自带 tag
/// 声明（如 TS 老族的 `双态 · 文件镜像`），其路径在聚合文档可能整族被内部
/// 剔除——留着会变成 UI 空分组（Swagger 渲染空区块）。声明的分组顺序对有
/// operation 的 tag 不受影响。
fn prune_empty_tags(document: &mut utoipa::openapi::OpenApi) {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in document.paths.paths.values() {
        let ops = [
            &item.get,
            &item.post,
            &item.put,
            &item.delete,
            &item.options,
            &item.head,
            &item.patch,
            &item.trace,
        ];
        for op in ops.into_iter().flatten() {
            if let Some(tags) = op.tags.as_ref() {
                used.extend(tags.iter().cloned());
            }
        }
    }
    if let Some(tags) = document.tags.as_mut() {
        tags.retain(|t| used.contains(&t.name));
    }
}

//! Scalar 文档 UI 静态资产：编译期内嵌 + 同源路由托管（离线/内网可用）。
//!
//! [`@scalar/api-reference`] 默认 HTML 从 jsdelivr CDN 加载 JS，JS 内的
//! `@font-face` 再从 fonts.scalar.com 拉取 Inter/mono 字体——浏览器不出网时
//! JS 加载失败直接白屏（字体缺失仅回退系统字体）。本模块将 standalone
//! bundle 与全部字体 vendor 进仓库（`assets/scalar/`，`include_bytes!`
//! 编译期内嵌二进制，与 Swagger UI 的 vendored 资产同策略），经同源路由
//! 提供；页面模板经 `Scalar::custom_html` 指向本地路径。bundle 内的字体
//! URL 已在 vendor 时改写为同源路径，升级流程见 `assets/scalar/vendor.sh`。

use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

/// vendored `@scalar/api-reference` 版本。升级资产时同步更新
/// （`assets/scalar/vendor.sh <version>`，bundle 路由 URL 含此版本）。
pub(super) const SCALAR_ASSET_VERSION: &str = "1.80.0";

/// Scalar 静态资产的同源托管前缀。字体 URL 改写（vendor.sh）硬编码引用
/// 此路径，两边必须一致。
const ASSETS_PREFIX: &str = "/api/docs/scalar/assets";

/// Scalar standalone bundle（@scalar/api-reference@1.80.0，MIT，见
/// `assets/scalar/LICENSE`）。二进制体积约 +5MB，换取文档页完全离线。
const SCALAR_BUNDLE: &[u8] = include_bytes!("../../assets/scalar/scalar-api-reference.js");

/// bundle `@font-face` 引用的全部 woff2 字体（Inter + JetBrains mono 两族
/// 的 unicode-range 分片）。
const SCALAR_FONTS: &[(&str, &[u8])] = &[
    (
        "inter-cyrillic-ext.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-cyrillic-ext.woff2"),
    ),
    (
        "inter-cyrillic.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-cyrillic.woff2"),
    ),
    (
        "inter-greek-ext.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-greek-ext.woff2"),
    ),
    (
        "inter-greek.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-greek.woff2"),
    ),
    (
        "inter-latin-ext.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-latin-ext.woff2"),
    ),
    (
        "inter-latin.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-latin.woff2"),
    ),
    (
        "inter-symbols.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-symbols.woff2"),
    ),
    (
        "inter-vietnamese.woff2",
        include_bytes!("../../assets/scalar/fonts/inter-vietnamese.woff2"),
    ),
    (
        "mono-cyrillic-ext.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-cyrillic-ext.woff2"),
    ),
    (
        "mono-cyrillic.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-cyrillic.woff2"),
    ),
    (
        "mono-greek.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-greek.woff2"),
    ),
    (
        "mono-latin-ext.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-latin-ext.woff2"),
    ),
    (
        "mono-latin.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-latin.woff2"),
    ),
    (
        "mono-vietnamese.woff2",
        include_bytes!("../../assets/scalar/fonts/mono-vietnamese.woff2"),
    ),
];

/// Scalar 页面模板：utoipa-scalar 默认模板的离线版。`$title`/`$spec` 占位符
/// 由 `Scalar::to_html` 填充（与默认模板同款）；script src 指向内嵌 bundle
/// 的版本化同源路由（内容变 ⇒ URL 变，immutable 缓存安全）。
pub(super) fn page_template() -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
    <title>$title</title>
    <meta charset="utf-8"/>
    <meta
            name="viewport"
            content="width=device-width, initial-scale=1"/>
</head>
<body>

<script
        id="api-reference"
        type="application/json">
    $spec
</script>
<script src="{ASSETS_PREFIX}/scalar-api-reference-{SCALAR_ASSET_VERSION}.js"></script>
</body>
</html>
"#,
    )
}

/// 组装静态资产路由（挂在 `/api/docs/scalar/assets` 下，与两个 Scalar 页面
/// 及 `/api/docs/{*rest}` 通配共存：matchit static 优先）。
pub(super) fn asset_router() -> axum::Router {
    axum::Router::new()
        .route(
            &format!("{ASSETS_PREFIX}/scalar-api-reference-{SCALAR_ASSET_VERSION}.js"),
            axum::routing::get(scalar_bundle_handler),
        )
        .route(
            &format!("{ASSETS_PREFIX}/fonts/{{file}}"),
            axum::routing::get(scalar_font_handler),
        )
}

/// 提供 vendored bundle：JS content type + 一年 immutable 缓存（版本化
/// URL 下内容不可变，缓存不会因升级返回陈旧脚本）。
async fn scalar_bundle_handler() -> Response {
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        SCALAR_BUNDLE,
    )
        .into_response()
}

/// 按 `{file}` 文件名提供字体；未知文件名明确 404（开放的字符串输入，
/// 不做模糊匹配兜底）。
async fn scalar_font_handler(Path(file): Path<String>) -> Response {
    let Some(bytes) = SCALAR_FONTS
        .iter()
        .find(|(name, _)| name == &file)
        .map(|(_, bytes)| *bytes)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (CONTENT_TYPE, "font/woff2"),
            (CACHE_CONTROL, "public, max-age=604800"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    /// 页面模板零外链：script src 指向版本化同源 bundle，不含任何
    /// http(s) 外网 URL，`$title`/`$spec` 占位符保留。
    #[test]
    fn page_template_points_to_local_bundle_only() {
        let html = page_template();
        let expected_src =
            format!("{ASSETS_PREFIX}/scalar-api-reference-{SCALAR_ASSET_VERSION}.js");
        assert!(
            html.contains(&format!(r#"<script src="{expected_src}"></script>"#)),
            "模板 script src 必须指向本地 bundle: {html}"
        );
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "页面模板不得包含外网资源 URL: {html}"
        );
        assert!(html.contains("$title"), "$title 占位符必须保留");
        assert!(html.contains("$spec"), "$spec 占位符必须保留");
    }

    /// bundle 本体零外链：vendor 改写后不得残留 jsdelivr / fonts.scalar.com。
    #[test]
    fn vendored_bundle_has_no_external_resource_urls() {
        let text = String::from_utf8_lossy(SCALAR_BUNDLE);
        for banned in ["jsdelivr", "fonts.scalar.com"] {
            assert!(
                !text.contains(banned),
                "vendored bundle 含外链 {banned}（vendor.sh 改写失效？）"
            );
        }
        assert!(
            text.starts_with("/*! Scalar API Reference"),
            "bundle 必须保留 vendor 溯源头注释"
        );
    }

    /// 字体清单与 bundle 引用对齐：bundle 改写后的字体引用数 = 清单条数，
    /// 文件名唯一且均为 woff2（防升级新增字体漏登记路由）。
    #[test]
    fn font_table_aligns_with_bundle_references() {
        let mut names: Vec<&str> = SCALAR_FONTS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "字体文件名重复: {names:?}");
        for name in &names {
            assert!(name.ends_with(".woff2"), "非 woff2 字体名: {name}");
        }
        let bundle = String::from_utf8_lossy(SCALAR_BUNDLE);
        // 只统计真实 @font-face 引用（`src: url(...)`），排除头注释里的路径字样
        let refs = bundle
            .matches(&format!("url({ASSETS_PREFIX}/fonts/"))
            .count();
        assert_eq!(
            refs,
            SCALAR_FONTS.len(),
            "bundle 字体引用数({refs})与路由清单({})不一致",
            SCALAR_FONTS.len()
        );
    }

    /// bundle 路由：200 + JS content type + immutable 缓存 + vendored 头。
    #[tokio::test]
    async fn bundle_route_serves_vendored_javascript() {
        let app = asset_router();
        let path =
            format!("/api/docs/scalar/assets/scalar-api-reference-{SCALAR_ASSET_VERSION}.js");
        let resp = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert!(
            resp.headers()[CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("immutable"),
            "bundle 响应必须 immutable 缓存"
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body.len(),
            SCALAR_BUNDLE.len(),
            "响应体与内嵌 bundle 不一致"
        );
        assert!(body.starts_with(b"/*! Scalar"), "响应体缺 vendored 头注释");
    }

    /// 字体路由：已知字体 200 + woff2 魔数；未知文件名 404。
    #[tokio::test]
    async fn font_route_serves_known_and_404s_unknown() {
        let app = asset_router();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/docs/scalar/assets/fonts/inter-latin.woff2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[CONTENT_TYPE], "font/woff2");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.starts_with(b"wOF2"), "woff2 魔数缺失");

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/docs/scalar/assets/fonts/nope.woff2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "未知字体必须 404");
    }
}

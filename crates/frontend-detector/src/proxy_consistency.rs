//! 产物路由一致性检查：推演 pingap 对静态产物的翻译链路。
//!
//! `[proxy].path` / `strip_prefix`（manifest 纸面规则）与构建工具烧进
//! `dist/index.html` 的资源引用路径（事实）错配时，部署后白屏——SPA fallback
//! 把资源 404 掩盖成 200，纯 toml 校验查不出（错配横跨 manifest 与
//! vite.config 两个文件）。本模块在编译期用规则纸面推演 pingap 的翻译链路：
//! 浏览器按引用发请求 → 命中 `[proxy].path` 路由 →（`strip_prefix` 则剥前缀）
//! → 静态托管从产物根取文件。引用"逃逸"出路由前缀、或翻译后的文件在产物中
//! 不存在，都是部署后必然白屏的信号，转为可操作的构建失败。
//!
//! 通道定位是尽力而为的观察（与本 crate 探测哲学一致）：无 index.html、读
//! 失败、提取不到绝对引用一律放行——漏报安全，绝不误报。合法双布局都认：
//! `strip=true` + 无前缀布局（`dist/assets/x.js` ← 引用 `/react/assets/x.js`）
//! 与 `strip=false` + 带前缀布局（`dist/react/assets/x.js` ← 同引用）。

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// `<script ... src="...">` 的资源引用。`[\s"']` 边界排除 `data-src` 类同尾缀
/// 属性名；`[^>]*` 含换行（标签可跨行）。
static SCRIPT_SRC_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"<script[^>]*[\s"']src=["']([^"']+)["']"#).ok());
/// `<link ... href="...">` 的资源引用（stylesheet/preload/icon 等统一取 href）。
static LINK_HREF_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"<link[^>]*[\s"']href=["']([^"']+)["']"#).ok());

/// 校验静态产物与 `[proxy]` 路由规则的一致性（编译期静态检查）。
///
/// - `dist_root`：静态产物目录（manifest `[build].artifact` 指向的目录本体）。
/// - `proxy_path`：`[proxy].path`（尾斜杠归一：`/react` ≡ `/react/`）。
/// - `strip_prefix`：`[proxy].strip_prefix`。
///
/// `Err` 为可直接指导修复的文案（指向构建工具 base/publicPath，或
/// `strip_prefix` 与产物布局的错配）；所有"查不了"的情形（无 index.html 的纯
/// 资源目录、读失败、无绝对引用）返回 `Ok(())`。script 引用先查、link 后查，
/// 报第一个错。
pub fn check_static_proxy_alignment(
    dist_root: &Path,
    proxy_path: &str,
    strip_prefix: bool,
) -> Result<(), String> {
    let path = normalize_proxy_path(proxy_path);
    // 校验层保证 path 以 `/` 开头；此处防御式 fail-open（观察通道不挡构建）
    if !path.starts_with('/') {
        return Ok(());
    }
    let index = dist_root.join("index.html");
    if !index.is_file() {
        return Ok(());
    }
    let Ok(html) = std::fs::read_to_string(&index) else {
        return Ok(());
    };
    let (Some(script_re), Some(link_re)) = (&*SCRIPT_SRC_RE, &*LINK_HREF_RE) else {
        return Ok(());
    };
    for re in [script_re, link_re] {
        for cap in re.captures_iter(&html) {
            if let Some(m) = cap.get(1) {
                check_reference(m.as_str(), &path, strip_prefix, dist_root)?;
            }
        }
    }
    Ok(())
}

/// 单条引用的翻译推演（只审绝对路径引用）。
fn check_reference(
    reference: &str,
    path: &str,
    strip_prefix: bool,
    dist_root: &Path,
) -> Result<(), String> {
    // 只审 `/` 开头的绝对引用：外链（http(s)/data）与相对路径（`./`）不经过
    // 路由前缀；`//cdn...` 是 protocol-relative 外链，同样不走路由。
    if !reference.starts_with('/') || reference.starts_with("//") {
        return Ok(());
    }
    // 逃逸检测（两种 strip 模式同判）：非前缀引用的浏览器请求落在路由外，
    // 任何 strip 配置都不可达（`strip=false` 时存在性检查可能恰好放行无前缀
    // 布局的文件，唯有逃逸判定能拦住）。段边界匹配防 `/react2/x.js` 误判。
    if !within_path_prefix(reference, path) {
        return Err(format!(
            "index.html 引用 {reference} 逃逸出 [proxy].path={path}——构建工具的 base/publicPath 应设为 {path}"
        ));
    }
    // app 根文档引用（= path 或 path/）：运行时由 SPA fallback 承载，不校验
    if reference == path || reference == format!("{path}/") {
        return Ok(());
    }
    // 翻译：strip=true 剥前缀（path="/" 时剥的是开头 `/`）；false 原样转发
    let translated: &str = if strip_prefix {
        if path == "/" {
            &reference[1..]
        } else {
            // within_path_prefix 已保证引用带完整段前缀（= path 的情形上方已跳过）
            &reference[path.len()..]
        }
    } else {
        reference
    };
    let rel = translated.trim_start_matches('/');
    if rel.is_empty() {
        return Ok(());
    }
    let file = dist_root.join(rel);
    if !file.is_file() {
        return Err(format!(
            "引用 {reference} 按 strip_prefix={strip_prefix} 翻译后找不到 {}——检查 [proxy].strip_prefix 与产物布局是否一致",
            file.display()
        ));
    }
    Ok(())
}

/// 段边界前缀判定：`ref == path`、或 `ref` 以 `{path}/` 开头；`path = "/"`
/// 恒真（catch-all 路由下所有绝对引用都在路由内）。
fn within_path_prefix(reference: &str, path: &str) -> bool {
    if path == "/" {
        return true;
    }
    reference == path
        || (reference.starts_with(path) && reference.as_bytes().get(path.len()) == Some(&b'/'))
}

/// 尾斜杠归一：`/react/` → `/react`；纯斜杠保持 `/`。
fn normalize_proxy_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// tempdir 造 dist fixture：index.html 内容 + 若干产物文件（相对 dist 根）。
    fn dist_fixture(html: &str, files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("index.html"), html).expect("write index.html");
        for f in files {
            let p = dir.path().join(f);
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(p, b"x").expect("write file");
        }
        dir
    }

    /// Form 1 正常通过：strip=true + 无前缀布局（vite 标准形态）。
    #[test]
    fn strip_true_unprefixed_layout_passes() {
        let dir = dist_fixture(
            r#"<html><head><link href="/react/assets/style.css" rel="stylesheet"></head>
                <body><script type="module" src="/react/assets/index.js"></script></body></html>"#,
            &["assets/index.js", "assets/style.css"],
        );
        assert!(check_static_proxy_alignment(dir.path(), "/react", true).is_ok());
    }

    /// base 逃逸：引用无前缀（base 写成 `/`），strip=true。
    #[test]
    fn unprefixed_reference_escapes() {
        let dir = dist_fixture(
            r#"<html><script src="/assets/index.js"></script></html>"#,
            &["assets/index.js"],
        );
        let err =
            check_static_proxy_alignment(dir.path(), "/react", true).expect_err("escape must fail");
        assert!(err.contains("逃逸"), "unexpected error: {err}");
        assert!(err.contains("base"), "unexpected error: {err}");
    }

    /// Form 2 正常通过：strip=false + 带前缀布局。
    #[test]
    fn strip_false_prefixed_layout_passes() {
        let dir = dist_fixture(
            r#"<html><script src="/react/assets/index.js"></script></html>"#,
            &["react/assets/index.js"],
        );
        assert!(check_static_proxy_alignment(dir.path(), "/react", false).is_ok());
    }

    /// strip=false + 无前缀布局：翻译后文件不存在 → 报错。
    #[test]
    fn strip_false_unprefixed_layout_fails() {
        let dir = dist_fixture(
            r#"<html><script src="/react/assets/index.js"></script></html>"#,
            &["assets/index.js"],
        );
        let err = check_static_proxy_alignment(dir.path(), "/react", false)
            .expect_err("missing file must fail");
        assert!(err.contains("strip_prefix"), "unexpected error: {err}");
        assert!(err.contains("找不到"), "unexpected error: {err}");
    }

    /// 硬化项：strip=false + 无前缀引用也判逃逸（请求落在路由外，布局恰好
    /// 含该文件时存在性检查会漏过）。
    #[test]
    fn strip_false_unprefixed_reference_escapes() {
        let dir = dist_fixture(
            r#"<html><script src="/assets/index.js"></script></html>"#,
            &["assets/index.js"],
        );
        let err = check_static_proxy_alignment(dir.path(), "/react", false)
            .expect_err("escape must fail under strip=false too");
        assert!(err.contains("逃逸"), "unexpected error: {err}");
    }

    /// 跳过态：无 index.html（纯资源目录）→ 放行。
    #[test]
    fn missing_index_html_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("assets")).expect("mkdir");
        fs::write(dir.path().join("assets/x.js"), b"x").expect("write");
        assert!(check_static_proxy_alignment(dir.path(), "/react", true).is_ok());
    }

    /// 跳过态：全外链/相对引用（无绝对引用）→ 放行（含 protocol-relative）。
    #[test]
    fn only_external_references_pass() {
        let dir = dist_fixture(
            r#"<html><head>
                <link href="https://cdn.example.com/x.css" rel="stylesheet">
                <link href="//cdn.example.com/y.css" rel="stylesheet">
                <link href="./local.css" rel="stylesheet">
                <script src="https://cdn.example.com/x.js"></script>
                <script src="rel.js"></script>
                </head></html>"#,
            &[],
        );
        assert!(check_static_proxy_alignment(dir.path(), "/react", true).is_ok());
    }

    /// 跳过态：空 html → 放行。
    #[test]
    fn empty_html_passes() {
        let dir = dist_fixture("", &[]);
        assert!(check_static_proxy_alignment(dir.path(), "/react", true).is_ok());
    }

    /// path 尾斜杠归一：`/react/` 与 `/react` 同结果。
    #[test]
    fn trailing_slash_normalized() {
        let html = r#"<html><script src="/react/assets/index.js"></script></html>"#;
        let ok_dir = dist_fixture(html, &["assets/index.js"]);
        assert!(check_static_proxy_alignment(ok_dir.path(), "/react/", true).is_ok());

        let bad_dir = dist_fixture(html, &[]);
        assert!(check_static_proxy_alignment(bad_dir.path(), "/react/", true).is_err());
    }

    /// 引用恰好等于 path 本身（app 根文档引用）→ 放行（SPA fallback 承载），
    /// 两种 strip 模式同判。
    #[test]
    fn reference_equal_to_path_passes() {
        for strip in [true, false] {
            let dir = dist_fixture(
                r#"<html><head><link href="/react"></head><body><script src="/react/"></script></body></html>"#,
                &["assets/index.js"],
            );
            assert!(check_static_proxy_alignment(dir.path(), "/react", strip).is_ok());
        }
    }

    /// 段边界：`/react2/x.js` 不算 `/react` 前缀 → 逃逸。
    #[test]
    fn sibling_segment_prefix_escapes() {
        let dir = dist_fixture(
            r#"<html><script src="/react2/x.js"></script></html>"#,
            &["react2/x.js", "assets/index.js"],
        );
        let err = check_static_proxy_alignment(dir.path(), "/react", true)
            .expect_err("sibling segment must escape");
        assert!(err.contains("逃逸"), "unexpected error: {err}");
    }

    /// catch-all：path = "/" 时所有绝对引用都在路由内，strip 语义按剥开头 `/` 推演。
    #[test]
    fn root_path_checks_layout() {
        let html = r#"<html><script src="/_next/static/x.js"></script></html>"#;
        let ok_dir = dist_fixture(html, &["_next/static/x.js"]);
        assert!(check_static_proxy_alignment(ok_dir.path(), "/", true).is_ok());

        let bad_dir = dist_fixture(html, &[]);
        assert!(check_static_proxy_alignment(bad_dir.path(), "/", true).is_err());
    }
}

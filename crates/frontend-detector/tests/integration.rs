//! 模板仓库实测：通过 RCODER_FRONTEND_TEMPLATE_DIR 指向已准备的模板仓库根目录。
//! 仅在对应模板文件存在时执行；未配置或缺目录时沿用既有门控，不能算真实模板验收。
//! fixture 单测覆盖主行为，这里验证真实项目目录的完整识别结果。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use frontend_detector::detect_project;

fn template_dir() -> Option<&'static Path> {
    static TEMPLATE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    TEMPLATE_DIR
        .get_or_init(|| {
            std::env::var_os("RCODER_FRONTEND_TEMPLATE_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .as_deref()
}

fn template_with_marker(sub: &str, marker: &str) -> Option<PathBuf> {
    let path = template_dir()?.join(sub);
    path.join(marker).is_file().then_some(path)
}

/// react 模板：build=vite（react/react-dom 在 node_modules 前提下版本实测，
/// 否则 range 提取）+ ui=react + pnpm + typescript。
#[test]
fn react_template_detected() {
    let Some(template) = template_with_marker("frontend-react-vite", "package.json") else {
        eprintln!(
            "skip: configure RCODER_FRONTEND_TEMPLATE_DIR with the prepared template; no real template acceptance was performed"
        );
        return;
    };
    let result = detect_project(&template);
    assert_eq!(result.build.name, "vite", "build: {:?}", result.build);
    assert_eq!(result.ui.name, "react", "ui: {:?}", result.ui);
    assert!(!result.ui.declared_range.is_empty(), "react 声明版本应带回");
    assert_eq!(result.package_manager.as_deref(), Some("pnpm"));
    assert!(result.typescript);
}

/// vue3 模板：build=vite + ui=vue3 + pnpm + typescript。
#[test]
fn vue3_template_detected() {
    let Some(template) = template_with_marker("frontend-vue3-vite", "package.json") else {
        eprintln!(
            "skip: configure RCODER_FRONTEND_TEMPLATE_DIR with the prepared template; no real template acceptance was performed"
        );
        return;
    };
    let result = detect_project(&template);
    assert_eq!(result.build.name, "vite");
    assert_eq!(result.ui.name, "vue3", "ui: {:?}", result.ui);
    assert_eq!(result.package_manager.as_deref(), Some("pnpm"));
    assert!(result.typescript);
}

/// next 模板：build=nextjs + ui=react 正交同真 + pnpm（pnpm-workspace.yaml
/// 优先于残留 package-lock.json——模板 24e412e 起安装链切 pnpm 11，
/// allowBuilds 白名单文件是更特异的锁信号）。
#[test]
fn next_template_detected() {
    let Some(template) = template_with_marker("userapp-next-template", "package.json") else {
        eprintln!(
            "skip: configure RCODER_FRONTEND_TEMPLATE_DIR with the prepared template; no real template acceptance was performed"
        );
        return;
    };
    let result = detect_project(&template);
    assert_eq!(result.build.name, "nextjs", "build: {:?}", result.build);
    assert_eq!(result.ui.name, "react", "ui: {:?}", result.ui);
    assert_eq!(result.package_manager.as_deref(), Some("pnpm"));
    assert!(result.typescript);
}

/// 非 Node 服务（java 后端目录）：探测面全降级。
#[test]
fn non_node_service_degrades_gracefully() {
    let Some(template) = template_with_marker("backend-java", "pom.xml") else {
        eprintln!(
            "skip: configure RCODER_FRONTEND_TEMPLATE_DIR with the prepared template; no real template acceptance was performed"
        );
        return;
    };
    let result = detect_project(&template);
    assert_eq!(result.build.name, "other");
    assert_eq!(result.ui.name, "other");
    assert_eq!(result.package_manager, None);
    assert!(!result.typescript);
}

//! 模板仓库实测（env 门控：本机存在模板目录才跑——CI/他机自动跳过；
//! fixture 单测已覆盖主行为，这里锁真实项目目录的完整识别结果）。

use frontend_detector::detect_project;

const TEMPLATE_DIR: &str = "/Users/soddy/Documents/git-workspace/userapp-workspace-template";

fn template_available(sub: &str) -> bool {
    std::path::Path::new(TEMPLATE_DIR)
        .join(sub)
        .join("package.json")
        .is_file()
}

/// react 模板：build=vite（react/react-dom 在 node_modules 前提下版本实测，
/// 否则 range 提取）+ ui=react + pnpm + typescript。
#[test]
fn react_template_detected() {
    if !template_available("frontend-react-vite") {
        eprintln!("skip: template dir not present");
        return;
    }
    let result = detect_project(
        std::path::Path::new(TEMPLATE_DIR)
            .join("frontend-react-vite")
            .as_path(),
    );
    assert_eq!(result.build.name, "vite", "build: {:?}", result.build);
    assert_eq!(result.ui.name, "react", "ui: {:?}", result.ui);
    assert!(!result.ui.declared_range.is_empty(), "react 声明版本应带回");
    assert_eq!(result.package_manager.as_deref(), Some("pnpm"));
    assert!(result.typescript);
}

/// vue3 模板：build=vite + ui=vue3 + pnpm + typescript。
#[test]
fn vue3_template_detected() {
    if !template_available("frontend-vue3-vite") {
        eprintln!("skip: template dir not present");
        return;
    }
    let result = detect_project(
        std::path::Path::new(TEMPLATE_DIR)
            .join("frontend-vue3-vite")
            .as_path(),
    );
    assert_eq!(result.build.name, "vite");
    assert_eq!(result.ui.name, "vue3", "ui: {:?}", result.ui);
    assert_eq!(result.package_manager.as_deref(), Some("pnpm"));
    assert!(result.typescript);
}

/// next 模板：build=nextjs + ui=react 正交同真 + npm（package-lock）。
#[test]
fn next_template_detected() {
    if !template_available("userapp-next-template") {
        eprintln!("skip: template dir not present");
        return;
    }
    let result = detect_project(
        std::path::Path::new(TEMPLATE_DIR)
            .join("userapp-next-template")
            .as_path(),
    );
    assert_eq!(result.build.name, "nextjs", "build: {:?}", result.build);
    assert_eq!(result.ui.name, "react", "ui: {:?}", result.ui);
    assert_eq!(result.package_manager.as_deref(), Some("npm"));
    assert!(result.typescript);
}

/// 非 Node 服务（java 后端目录）：探测面全降级。
#[test]
fn non_node_service_degrades_gracefully() {
    if !std::path::Path::new(TEMPLATE_DIR)
        .join("backend-java/pom.xml")
        .is_file()
    {
        eprintln!("skip: template dir not present");
        return;
    }
    let result = detect_project(
        std::path::Path::new(TEMPLATE_DIR)
            .join("backend-java")
            .as_path(),
    );
    assert_eq!(result.build.name, "other");
    assert_eq!(result.ui.name, "other");
    assert_eq!(result.package_manager, None);
    assert!(!result.typescript);
}

//! 前端项目框架探测（纯函数、无副作用、零业务依赖）。
//!
//! 识别一个前端项目目录：
//! - **build/meta 框架**（vite/nextjs/nuxt/astro/...，单结果清单序优先）
//! - **UI 框架**（react/vue3/vue2/svelte/...，与 build 维度正交可同真）
//! - 每维度**框架版本**（三级口径：node_modules 实测 > 精确声明 > range 提取）
//! - **包管理器**（packageManager 字段 > lockfile 存在性）
//! - **typescript** 判定
//!
//! 规则数据来源：Vercel `fs-detectors` 与 Netlify `build-info` 源码逐条核实
//!（非自创）；设计取舍见 [`engine`] 模块文档。**扩展方法**：新框架 =
//! [`rules`] 加一行 + 一条单测，消费方零改动。
//!
//! 探测是尽力而为的观察通道：package.json 缺失或损坏、node_modules 部分安装
//! 均优雅降级（other / declared 口径），不报错不 panic。

mod engine;
mod package_json;
mod rules;
mod version;

pub use engine::{
    FrameworkHit, detect_build, detect_package_manager, detect_typescript, detect_ui,
};
pub use version::VersionSource;

/// 单个项目的完整探测结果（领域结构；wire 序列化归消费方壳层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFrameworks {
    /// 构建 / meta 框架（未识别 name = "other"）。
    pub build: FrameworkHit,
    /// UI 框架（与 build 正交；未识别 name = "other"）。
    pub ui: FrameworkHit,
    /// 包管理器（pnpm/npm/yarn/bun；无任何信号为 None——如无 package.json 的
    /// 非 Node 项目）。
    pub package_manager: Option<String>,
    /// 项目使用 TypeScript（typescript 依赖 ∨ tsconfig.json 存在）。
    pub typescript: bool,
}

/// 探测一个项目目录（同步、毫秒级；目录无 package.json → 探测面全降级）。
pub fn detect_project(dir: &std::path::Path) -> ProjectFrameworks {
    let Some(pkg) = package_json::PackageJsonMinimal::read(dir) else {
        return ProjectFrameworks {
            build: FrameworkHit::unresolved_other(),
            ui: FrameworkHit::unresolved_other(),
            package_manager: None,
            typescript: false,
        };
    };
    ProjectFrameworks {
        build: detect_build(dir, &pkg),
        ui: detect_ui(dir, &pkg),
        package_manager: detect_package_manager(dir, &pkg),
        typescript: detect_typescript(dir, &pkg),
    }
}

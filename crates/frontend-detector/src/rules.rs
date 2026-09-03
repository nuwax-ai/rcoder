//! 静态规则表（纯数据）。规则来源：Vercel `fs-detectors` 与 Netlify
//! `build-info` 源码逐条核实（见各条注释），非自创。
//!
//! **扩展方法**：新框架 = 加一行规则 + 一条单测。注意清单序即优先级
//!（元框架排在裸构建器之前，如 SvelteKit 先于 Vite——SvelteKit 项目依赖
//! vite，靠序位消解冲突，无需显式 supersedes）；`excluded` 用于把"未收录
//! 的元框架依赖"从兜底规则（vite/webpack）中排除，防误报。

/// 构建 / meta 框架规则（依赖为主信号；配置文件存在性不进规则——零配置
/// 项目也必声明依赖，依赖判定更强）。
pub struct BuildRule {
    pub name: &'static str,
    pub display_name: &'static str,
    /// 任一依赖存在即候选；**首个命中的依赖名负责版本提取**（顺序有意义）。
    pub packages_some: &'static [&'static str],
    /// 必须全部存在的附加依赖（AND，如 solid-start 需 solid-js + @solidjs/start）。
    pub require_all: &'static [&'static str],
    /// 任一存在即否决（Netlify excludedNpmDependencies 语义：vite 被众多
    /// 元框架携带，兜底命中前先排除）。
    pub excluded: &'static [&'static str],
}

/// UI 框架（库级）规则——与 build 维度**正交**（next 项目 build=nextjs +
/// ui=react 同真）。Vercel 设计教训：UI 库依赖会被所有同生态 meta 框架携带，
/// 这正是正交双维度的价值而非缺陷。
pub struct UiRule {
    pub name: &'static str,
    pub display_name: &'static str,
    /// 任一依赖存在即命中；首个命中的依赖名负责版本提取。
    pub packages_some: &'static [&'static str],
}

/// build 框架清单（**清单序即优先级**，元框架在前、裸构建器兜底在后）。
pub static BUILD_RULES: &[BuildRule] = &[
    BuildRule {
        name: "nextjs",
        display_name: "Next.js",
        packages_some: &["next"],
        require_all: &[],
        excluded: &[],
    },
    // Nuxt some 列表含 nightly/edge 变体（Vercel 原表）
    BuildRule {
        name: "nuxt",
        display_name: "Nuxt",
        packages_some: &["nuxt", "nuxt3", "nuxt-edge", "nuxt-nightly"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "remix",
        display_name: "Remix",
        packages_some: &["@remix-run/dev", "@remix-run/react", "remix"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "astro",
        display_name: "Astro",
        packages_some: &["astro"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "sveltekit",
        display_name: "SvelteKit",
        packages_some: &["@sveltejs/kit"],
        require_all: &[],
        excluded: &[],
    },
    // Vercel：every solid-js AND @solidjs/start
    BuildRule {
        name: "solid-start",
        display_name: "SolidStart",
        packages_some: &["@solidjs/start"],
        require_all: &["solid-js"],
        excluded: &[],
    },
    BuildRule {
        name: "gatsby",
        display_name: "Gatsby",
        packages_some: &["gatsby"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "docusaurus",
        display_name: "Docusaurus",
        packages_some: &["@docusaurus/core"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "angular",
        display_name: "Angular",
        packages_some: &["@angular/cli"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "create-react-app",
        display_name: "Create React App",
        packages_some: &["react-scripts", "react-dev-utils"],
        require_all: &[],
        excluded: &[],
    },
    // Vercel 原注释：特意不用 `vue` 包名（UI 库会被其它框架携带），用 CLI 依赖
    BuildRule {
        name: "vue-cli",
        display_name: "Vue CLI",
        packages_some: &["@vue/cli-service"],
        require_all: &[],
        excluded: &[],
    },
    BuildRule {
        name: "rsbuild",
        display_name: "Rsbuild",
        packages_some: &["rsbuild", "@rspack/cli"],
        require_all: &[],
        excluded: &[],
    },
    // Vite 兜底 + Netlify 排除表（元框架声明 vite 依赖，未收录前先排除防误报）
    BuildRule {
        name: "vite",
        display_name: "Vite",
        packages_some: &["vite"],
        require_all: &[],
        excluded: &[
            "@sveltejs/kit",
            "@remix-run/dev",
            "@shopify/hydrogen",
            "@builder.io/qwik",
            "solid-start",
            "@solidjs/start",
            "@tanstack/start",
            "@react-router/dev",
            "vike",
        ],
    },
    BuildRule {
        name: "webpack",
        display_name: "webpack",
        packages_some: &["webpack"],
        require_all: &[],
        excluded: &[],
    },
];

/// UI 框架清单（顺序即优先级；正常项目至多命中一个，react 与 vue 不共存）。
/// vue 命中后由引擎按版本细分为 vue3/vue2/vue（对齐 nuwax 三态语义）。
pub static UI_RULES: &[UiRule] = &[
    UiRule {
        name: "react",
        display_name: "React",
        packages_some: &["react", "react-dom"],
    },
    UiRule {
        name: "vue",
        display_name: "Vue",
        // 版本候选顺序 vue → vue-router → @vue/cli-service（nuwax 语义）：
        // 第一个可解析主版本的依赖决定 vue2/vue3
        packages_some: &["vue", "vue-router", "@vue/cli-service"],
    },
    UiRule {
        name: "svelte",
        display_name: "Svelte",
        packages_some: &["svelte"],
    },
    UiRule {
        name: "solid",
        display_name: "SolidJS",
        packages_some: &["solid-js"],
    },
    UiRule {
        name: "preact",
        display_name: "Preact",
        packages_some: &["preact"],
    },
    UiRule {
        name: "angular",
        display_name: "Angular",
        packages_some: &["@angular/core"],
    },
];

/// lockfile 文件名 → 包管理器（顺序即检测优先，更特异的在前；
/// antfu package-manager-detector LOCKS 表）。
pub static LOCKFILE_TO_PM: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm"),
    ("pnpm-workspace.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
    ("npm-shrinkwrap.json", "npm"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单序完整性：元框架必须在 vite/webpack 之前（序位消解依赖此约定）。
    #[test]
    fn build_rules_meta_frameworks_precede_bare_builders() {
        let vite_index = BUILD_RULES
            .iter()
            .position(|rule| rule.name == "vite")
            .expect("vite rule");
        let webpack_index = BUILD_RULES
            .iter()
            .position(|rule| rule.name == "webpack")
            .expect("webpack rule");
        for meta in ["nextjs", "nuxt", "remix", "sveltekit", "solid-start"] {
            let index = BUILD_RULES
                .iter()
                .position(|rule| rule.name == meta)
                .expect("meta rule");
            assert!(index < vite_index, "{meta} 必须排在 vite 之前（序位消解）");
            assert!(index < webpack_index, "{meta} 必须排在 webpack 之前");
        }
    }

    /// 规则名唯一（引擎按名语义不受重复干扰）。
    #[test]
    fn rule_names_are_unique() {
        let mut names: Vec<&str> = BUILD_RULES.iter().map(|r| r.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "build 规则名不得重复");
        let mut ui_names: Vec<&str> = UI_RULES.iter().map(|r| r.name).collect();
        let ui_total = ui_names.len();
        ui_names.sort_unstable();
        ui_names.dedup();
        assert_eq!(ui_names.len(), ui_total, "ui 规则名不得重复");
    }
}

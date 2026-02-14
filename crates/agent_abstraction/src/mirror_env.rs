//! 镜像源环境变量透传
//!
//! 收集当前进程中的镜像源配置，供 MCP 子进程（npx/bunx/uvx）使用。
//!
//! 变量来源优先级：
//! 1. 直接变量（npm_config_registry / UV_INDEX_URL / PIP_INDEX_URL）
//! 2. MCP_PROXY_* 中转变量（MCP_PROXY_NPM_REGISTRY / MCP_PROXY_PYPI_INDEX_URL）

/// 需要透传的镜像源环境变量映射
///
/// 格式: (目标变量名, 后备变量名)
const MIRROR_MAPPINGS: &[(&str, &str)] = &[
    ("npm_config_registry", "MCP_PROXY_NPM_REGISTRY"),
    ("UV_INDEX_URL", "MCP_PROXY_PYPI_INDEX_URL"),
    ("PIP_INDEX_URL", "MCP_PROXY_PYPI_INDEX_URL"),
];

/// 收集需要透传给子进程的镜像源环境变量
///
/// 每个目标变量优先读取自身，未设置时从对应的 MCP_PROXY_* 后备变量转换。
/// 返回 `Vec<(key, value)>`，仅包含有值的条目。
pub fn collect_mirror_env_vars() -> Vec<(String, String)> {
    MIRROR_MAPPINGS
        .iter()
        .filter_map(|&(target, fallback)| {
            let val = std::env::var(target)
                .or_else(|_| std::env::var(fallback))
                .ok()?;
            Some((target.to_string(), val))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_does_not_panic() {
        let _ = collect_mirror_env_vars();
    }

    #[test]
    fn mappings_have_correct_targets() {
        // 确保目标变量名是工具直接使用的名称
        assert!(MIRROR_MAPPINGS.iter().any(|(t, _)| *t == "npm_config_registry"));
        assert!(MIRROR_MAPPINGS.iter().any(|(t, _)| *t == "UV_INDEX_URL"));
        assert!(MIRROR_MAPPINGS.iter().any(|(t, _)| *t == "PIP_INDEX_URL"));
    }
}

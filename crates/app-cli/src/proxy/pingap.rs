//! 用 `pingap-config` crate 官方类型组装 pingap 配置 + `toml` 序列化。
//!
//! pingap-config 的 bollard/regex 依赖隔离在 app-cli 自己的 Cargo.lock（app-cli 独立于 rcoder workspace）。

use pingap_config::{LocationConf, PingapConfig, ServerConf, UpstreamConf};
use workspace_manifest::ProxySection;

/// pingap 监听端口（不用 3000，前端框架默认端口；9080 无冲突）。
pub const PINGAP_PORT: u16 = 9080;

/// 错误来源剥离插件名（managed/extend 生成的普通响应插件链末尾移除应用
/// 响应中的 `X-Pingap-EType` 头；pingap 自产 fail_to_proxy 错误不走插件链、
/// 头保留——rcoder 据此区分「代理自产错误」与「应用自己返回的错误正文」）。
/// 实测依据：锁定 Pingap cd74a461（v0.14.3）二进制协议验证（2026-09-26）。
pub const STRIP_ERROR_ORIGIN_PLUGIN: &str = "rcoderStripErrorOrigin";

/// workspace 首页静态服务的 pingap upstream / location 名（注入兜底路由用）。
pub const WORKSPACE_INDEX_UPSTREAM: &str = "workspaceIndex";
pub const WORKSPACE_INDEX_LOCATION: &str = "workspaceIndexLocation";

/// 一个需要代理的子项目（从 ServiceSpec 提取，pingap_config 不依赖 manifest 模块）。
pub struct ProxyEntry {
    pub name: String,
    pub port: u16,
    pub proxy: ProxySection,
    /// 健康检查路径（pingap upstream health_check 用它探活）。
    pub health: String,
}

/// 从 service specs 中提取有 [proxy] 的项目 → 生成 pingap 配置。
///
/// 仅当 ≥1 子项目有 [proxy] 时返回 `Some`。约定：
/// - pingap 监听 :9080；各子项目 upstream = `127.0.0.1:<port>`。
/// - `proxy.path == "/"` 兜底（location 不写 path）；其余前缀匹配；`strip_prefix` 去前缀。
/// - 每个 location 默认带 `pingap:requestId` + `pingap:compressionUpstream`（零配置内置插件）。
/// - `index_port = Some` 时追加 workspace 首页静态服务（`app-cli workspace_index`）的
///   兜底路由：无 path location（权重 0，服务前缀路由权重 512+ 恒优先）。调用方须
///   保证仅在无服务占 `/` 时传 Some（两个无 path location 并存同权重有匹配歧义）。
pub fn build_pingap_config(
    entries: &[ProxyEntry],
    index_port: Option<u16>,
) -> anyhow::Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }

    let mut cfg = PingapConfig::default();

    // [servers.app]
    let mut location_names: Vec<String> = entries
        .iter()
        .map(|e| format!("{}Location", e.name))
        .collect();
    if index_port.is_some() {
        location_names.push(WORKSPACE_INDEX_LOCATION.into());
    }
    cfg.servers.insert(
        "app".into(),
        ServerConf {
            addr: format!("0.0.0.0:{PINGAP_PORT}"),
            locations: Some(location_names),
            ..Default::default()
        },
    );

    // workspace 首页静态服务（index.html）：兜底 upstream + 无 path location。
    if let Some(port) = index_port {
        cfg.upstreams.insert(
            WORKSPACE_INDEX_UPSTREAM.into(),
            UpstreamConf {
                addrs: vec![format!("127.0.0.1:{port}")],
                health_check: Some(format!("http://{WORKSPACE_INDEX_UPSTREAM}/")),
                ..Default::default()
            },
        );
        cfg.locations.insert(
            WORKSPACE_INDEX_LOCATION.into(),
            LocationConf {
                upstream: Some(WORKSPACE_INDEX_UPSTREAM.into()),
                plugins: Some(vec!["pingap:requestId".into()]),
                // 不写 path：pingap 默认权重 0 = 兜底（前缀路由恒优先）
                ..Default::default()
            },
        );
    }

    // 错误来源剥离插件定义（response_headers · 移除应用响应的同名头）。
    // PluginConf 与本 crate 的 serde_json 可能来自不同版本实例——经 serde
    // 解析构造（跨版本安全），不直接组装 Map。
    let strip_plugin: pingap_config::PluginConf = serde_json::from_str(
        r#"{"category":"response_headers","remove_headers":["X-Pingap-EType"]}"#,
    )?;
    cfg.plugins
        .insert(STRIP_ERROR_ORIGIN_PLUGIN.into(), strip_plugin);

    // 每个 proxied 项目：[upstreams.<name>] + [locations.<name>Location]
    for e in entries {
        let name = e.name.clone();
        cfg.upstreams.insert(
            name.clone(),
            UpstreamConf {
                addrs: vec![format!("127.0.0.1:{}", e.port)],
                health_check: Some(format!("http://{name}{}", e.health)),
                ..Default::default()
            },
        );

        // 平台内置插件 + manifest 显式引用的插件 + 错误来源剥离（响应链末尾：
        // 应用同名响应头被移除，代理自产错误头保留——见常量注释）。
        let mut plugins: Vec<String> = vec!["pingap:requestId".into()];
        plugins.extend(e.proxy.plugins.clone());
        plugins.push("pingap:compressionUpstream".into());
        plugins.push(STRIP_ERROR_ORIGIN_PLUGIN.into());

        let is_catchall = e.proxy.path == "/";
        let mut loc = LocationConf {
            upstream: Some(name.clone()),
            plugins: Some(plugins),
            // 不用 enable_reverse_proxy_headers 默认集：它会设 `x-forwarded-host:$host`，
            // 而 pingap 的 `$host` 刻意去端口（仿 nginx），非标准端口下与浏览器 origin
            // （含端口）不一致，导致 Next.js/Nuxt 等 Server Actions 的 CSRF origin 校验失败。
            // 改自定义 proxy_set_headers：保留 for/proto（后端取 client IP/scheme），
            // 故意不设 x-forwarded-host —— 让后端读 host header（pingap/pingora 保留客户端
            // 原始 host:port），与浏览器 origin 一致，Server Actions 校验通过。
            enable_reverse_proxy_headers: Some(false),
            proxy_set_headers: Some(vec![
                "x-real-ip:$remote_addr".into(),
                "x-forwarded-for:$proxy_add_x_forwarded_for".into(),
                "x-forwarded-proto:$scheme".into(),
            ]),
            ..Default::default()
        };
        if !is_catchall {
            loc.path = Some(e.proxy.path.clone());
            if e.proxy.strip_prefix {
                // path 是字面前缀，须正则转义：元字符（如 `(`、`|`）裸拼会改变捕获组
                // 编号或把正则切成 alternation，路由静默错乱（校验层只拒绝 `..`/`?`/`#`，
                // 放行这些元字符——生成层必须自守）
                // Consume exactly the separator after the prefix. Otherwise a
                // slashless prefix turns /react/assets/x into //assets/x.
                let prefix = e.proxy.path.trim_end_matches('/');
                loc.rewrite = Some(format!("^{}(?:/|$)(.*) /$1", regex::escape(prefix)));
            }
        }
        cfg.locations.insert(format!("{name}Location"), loc);
    }

    Ok(Some(toml::to_string(&cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, port: u16, path: &str, strip: bool) -> ProxyEntry {
        ProxyEntry {
            name: name.into(),
            port,
            proxy: ProxySection {
                path: path.into(),
                strip_prefix: strip,
                dev_strip_prefix: None,
                plugins: Vec::new(),
                upstream_includes: Vec::new(),
            },
            health: "/health".into(),
        }
    }

    #[test]
    fn empty_entries_returns_none() {
        assert!(build_pingap_config(&[], None).expect("serialize").is_none());
    }

    /// index_port=Some 时注入 workspaceIndex 兜底：upstream + 无 path location +
    /// server locations 挂载；None 时不出现。
    #[test]
    fn index_upstream_injected_only_when_port_given() {
        let entries = vec![entry("frontend", 4000, "/react", true)];

        let with_index = build_pingap_config(&entries, Some(9081))
            .expect("serialize")
            .expect("non-empty config");
        assert!(
            with_index.contains("[upstreams.workspaceIndex]"),
            "{with_index}"
        );
        assert!(
            with_index.contains("addrs = [\"127.0.0.1:9081\"]"),
            "{with_index}"
        );
        assert!(
            with_index.contains("[locations.workspaceIndexLocation]"),
            "{with_index}"
        );
        // 兜底 location 自身不写 path（pingap 默认权重 0）。块内容以段头截断——
        // PingapConfig 的 locations 是 HashMap，toml 输出序不定，块后可能紧跟
        // 其他 location（其 path 不得误伤本断言）。
        let location_body = with_index
            .split("[locations.workspaceIndexLocation]")
            .nth(1)
            .expect("location block")
            .split('[')
            .next()
            .unwrap_or("");
        assert!(!location_body.contains("path ="), "{with_index}");

        let without_index = build_pingap_config(&entries, None)
            .expect("serialize")
            .expect("non-empty config");
        assert!(!without_index.contains("workspaceIndex"), "{without_index}");
    }

    #[test]
    fn routes_root_and_api_with_builtin_plugins() {
        let entries = vec![
            entry("frontend", 4000, "/", false),
            entry("backend", 4001, "/api/", true),
        ];
        let toml_text = build_pingap_config(&entries, None)
            .expect("serialize")
            .expect("non-empty config");
        assert!(toml_text.contains("addr = \"0.0.0.0:9080\""), "{toml_text}");
        assert!(toml_text.contains("[upstreams.frontend]"), "{toml_text}");
        assert!(
            toml_text.contains("addrs = [\"127.0.0.1:4000\"]"),
            "{toml_text}"
        );
        assert!(
            toml_text.contains("[locations.frontendLocation]"),
            "{toml_text}"
        );
        assert!(
            toml_text.contains("addrs = [\"127.0.0.1:4001\"]"),
            "{toml_text}"
        );
        assert!(toml_text.contains("path = \"/api/\""), "{toml_text}");
        // regex::escape 只转义真元字符，`/` 不在其中——普通路径输出不变
        assert!(
            toml_text.contains("rewrite = \"^/api(?:/|$)(.*) /$1\""),
            "{toml_text}"
        );
        assert!(toml_text.contains("\"pingap:requestId\""), "{toml_text}");
        assert!(
            toml_text.contains("\"pingap:compressionUpstream\""),
            "{toml_text}"
        );
    }

    /// 错误来源剥离插件：定义存在 + 挂在每个 proxied location 的响应链末尾。
    #[test]
    fn strip_error_origin_plugin_is_declared_and_attached() {
        let entries = vec![entry("frontend", 4000, "/", false)];
        let toml_text = build_pingap_config(&entries, None)
            .expect("serialize")
            .expect("non-empty config");
        assert!(
            toml_text.contains("[plugins.rcoderStripErrorOrigin]"),
            "{toml_text}"
        );
        assert!(
            toml_text.contains(r#"remove_headers = ["X-Pingap-EType"]"#),
            "{toml_text}"
        );
        assert!(
            toml_text.contains("category = \"response_headers\""),
            "{toml_text}"
        );
        let parsed: toml::Value = toml::from_str(&toml_text).expect("reparse");
        let plugins = parsed["locations"]["frontendLocation"]["plugins"]
            .as_array()
            .expect("plugins list");
        assert_eq!(
            plugins.last().and_then(|p| p.as_str()),
            Some(STRIP_ERROR_ORIGIN_PLUGIN)
        );
    }

    /// path 是字面前缀：元字符必须转义，否则 `v(1)` 会把 `$1` 捕获组从 `(.*)`
    /// 漂移到 `(1)`，所有请求被改写成组内字面量——路由静默错乱。
    #[test]
    fn rewrite_escapes_regex_metacharacters_in_path() {
        let entries = vec![entry("backend", 4001, "/api/v(1)/", true)];
        let toml_text = build_pingap_config(&entries, None)
            .expect("serialize")
            .expect("non-empty config");
        let parsed: toml::Value = toml::from_str(&toml_text).expect("reparse generated config");
        let rewrite = parsed["locations"]["backendLocation"]["rewrite"]
            .as_str()
            .expect("rewrite field");
        // 语义验证：pattern（空格前段）按 Rust regex 解析，捕获组 1 必须恒为尾部 (.*)，
        // 且字面路径（含元字符原样）能命中
        let pattern = rewrite.split(' ').next().expect("pattern segment");
        let re = regex::Regex::new(pattern).expect("rewrite pattern must compile");
        let caps = re.captures("/api/v(1)/x").expect("literal path must match");
        assert_eq!(caps.get(1).map(|m| m.as_str()), Some("x"));
        assert_eq!(
            caps.get(2),
            None,
            "metacharacters must not add capture groups"
        );
    }

    #[test]
    fn strip_rewrite_preserves_resource_paths_with_or_without_trailing_slash() {
        for prefix in ["/react", "/react/", "/api/v(1)", "/api/v(1)/"] {
            let config = build_pingap_config(&[entry("web", 4000, prefix, true)], None)
                .unwrap()
                .unwrap();
            let config = PingapConfig::new(config.as_bytes(), true).unwrap();
            let rewrite = config.locations["webLocation"].rewrite.as_ref().unwrap();
            let (pattern, replacement) = rewrite.split_once(' ').unwrap();
            let regex = regex::Regex::new(pattern).unwrap();
            let base = prefix.trim_end_matches('/');
            for (suffix, expected) in [
                ("", "/"),
                ("/", "/"),
                ("/@vite/client", "/@vite/client"),
                ("/assets/index.js", "/assets/index.js"),
                ("/health", "/health"),
            ] {
                let path = format!("{base}{suffix}");
                assert_eq!(
                    regex.replace(&path, replacement),
                    expected,
                    "{prefix}: {path}"
                );
            }
            let unrelated = format!("{base}ive/main.js");
            assert!(
                !regex.is_match(&unrelated),
                "must respect a path segment boundary"
            );
        }
    }
}

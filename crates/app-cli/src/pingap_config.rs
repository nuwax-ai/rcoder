//! 用 `pingap-config` crate 官方类型组装 pingap 配置 + `toml` 序列化。
//!
//! pingap-config 的 bollard/regex 依赖隔离在 app-cli 自己的 Cargo.lock（app-cli 独立于 rcoder workspace）。

use pingap_config::{LocationConf, PingapConfig, ServerConf, UpstreamConf};
use workspace_manifest::ProxySection;

/// pingap 监听端口（不用 3000，前端框架默认端口；9080 无冲突）。
pub const PINGAP_PORT: u16 = 9080;

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
pub fn build_pingap_config(entries: &[ProxyEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut cfg = PingapConfig::default();

    // [servers.app]
    let location_names: Vec<String> = entries
        .iter()
        .map(|e| format!("{}Location", e.name))
        .collect();
    cfg.servers.insert(
        "app".into(),
        ServerConf {
            addr: format!("0.0.0.0:{PINGAP_PORT}"),
            locations: Some(location_names),
            ..Default::default()
        },
    );

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
        let is_catchall = e.proxy.path == "/";
        let mut loc = LocationConf {
            upstream: Some(name.clone()),
            plugins: Some(vec![
                "pingap:requestId".into(),
                "pingap:compressionUpstream".into(),
            ]),
            enable_reverse_proxy_headers: Some(true),
            ..Default::default()
        };
        if !is_catchall {
            loc.path = Some(e.proxy.path.clone());
            if e.proxy.strip_prefix.unwrap_or(false) {
                loc.rewrite = Some(format!("^{}(.*) /$1", e.proxy.path));
            }
        }
        cfg.locations.insert(format!("{name}Location"), loc);
    }

    toml::to_string(&cfg).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, port: u16, path: &str, strip: Option<bool>) -> ProxyEntry {
        ProxyEntry {
            name: name.into(),
            port,
            proxy: ProxySection {
                path: path.into(),
                strip_prefix: strip,
                cache: None,
                rate_limit: None,
            },
            health: "/health".into(),
        }
    }

    #[test]
    fn empty_entries_returns_none() {
        assert!(build_pingap_config(&[]).is_none());
    }

    #[test]
    fn routes_root_and_api_with_builtin_plugins() {
        let entries = vec![
            entry("frontend", 4000, "/", None),
            entry("backend", 4001, "/api/", Some(true)),
        ];
        let toml_text = build_pingap_config(&entries).expect("→ Some");
        assert!(toml_text.contains("addr = \"0.0.0.0:9080\""), "{toml_text}");
        assert!(toml_text.contains("[upstreams.frontend]"), "{toml_text}");
        assert!(toml_text.contains("addrs = [\"127.0.0.1:4000\"]"), "{toml_text}");
        assert!(toml_text.contains("[locations.frontendLocation]"), "{toml_text}");
        assert!(toml_text.contains("addrs = [\"127.0.0.1:4001\"]"), "{toml_text}");
        assert!(toml_text.contains("path = \"/api/\""), "{toml_text}");
        assert!(toml_text.contains("rewrite = \"^/api/(.*) /$1\""), "{toml_text}");
        assert!(toml_text.contains("\"pingap:requestId\""), "{toml_text}");
        assert!(
            toml_text.contains("\"pingap:compressionUpstream\""),
            "{toml_text}"
        );
    }
}

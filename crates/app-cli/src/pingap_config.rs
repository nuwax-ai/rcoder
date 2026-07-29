//! 用 `pingap-config` crate 官方类型组装 pingap 配置 + `toml` 序列化。
//!
//! 类型安全 + 与 pingap schema 一致、随 pingap 版本演进。
//! pingap-config 的 bollard/regex 依赖隔离在 app-cli 自己的 Cargo.lock（app-cli 独立于 rcoder workspace）。

use pingap_config::{LocationConf, PingapConfig, ServerConf, UpstreamConf};
use workspace_manifest::ProjectRef;

/// pingap 监听端口（= `[deploy].ports` 的 app HTTP 端口，rcoder pingora 透出）。
/// 不用 3000（前端框架默认端口）；9080 无冲突。
pub const PINGAP_PORT: u16 = 9080;
/// 子项目内部端口基（4000+i，i = `[[projects]]` 顺序下标）。
pub const INTERNAL_PORT_BASE: u16 = 4000;

/// 按 workspace manifest 的 `[[projects]].proxy_path` 生成 pingap 配置（pingap.toml 文本）。
///
/// 仅当 ≥1 子项目声明 `proxy_path` 时返回 `Some`。约定：
/// - pingap 监听 :9080；各子项目 upstream = `127.0.0.1:<4000+i>`。
/// - `proxy_path == "/"` 兜底（location 不写 path）；其余前缀匹配；`proxy_strip_prefix` 去前缀。
/// - 每个 location 默认带 `pingap:requestId` + `pingap:compressionUpstream`（零配置内置插件）。
pub fn build_pingap_config(projects: &[ProjectRef]) -> Option<String> {
    let proxied: Vec<(usize, &ProjectRef)> = projects
        .iter()
        .enumerate()
        .filter(|(_, p)| p.proxy_path.as_deref().is_some_and(|s| !s.is_empty()))
        .collect();
    if proxied.is_empty() {
        return None;
    }

    let mut cfg = PingapConfig::default();

    // [servers.app]
    let location_names: Vec<String> = proxied
        .iter()
        .map(|(_, p)| format!("{}Location", p.name))
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
    for (idx, p) in &proxied {
        let port = INTERNAL_PORT_BASE + *idx as u16;
        let name = p.name.clone();
        cfg.upstreams.insert(
            name.clone(),
            UpstreamConf {
                addrs: vec![format!("127.0.0.1:{port}")],
                ..Default::default()
            },
        );
        let is_catchall = p.proxy_path.as_deref() == Some("/");
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
            if let Some(path) = p.proxy_path.as_deref() {
                loc.path = Some(path.to_string());
                if p.proxy_strip_prefix.unwrap_or(false) {
                    loc.rewrite = Some(format!("^{path}(.*) /$1"));
                }
            }
        }
        cfg.locations.insert(format!("{name}Location"), loc);
    }

    toml::to_string(&cfg).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_manifest::ProjectRef;

    fn proj(name: &str, proxy_path: Option<&str>, strip: Option<bool>) -> ProjectRef {
        ProjectRef {
            name: name.into(),
            path: name.into(),
            proxy_path: proxy_path.map(String::from),
            proxy_strip_prefix: strip,
            proxy_cache: None,
            proxy_rate_limit: None,
        }
    }

    #[test]
    fn none_when_no_proxy() {
        let projects = vec![proj("a", None, None)];
        assert!(build_pingap_config(&projects).is_none());
    }

    #[test]
    fn routes_root_and_api_with_builtin_plugins() {
        let projects = vec![
            proj("frontend", Some("/"), None),
            proj("backend", Some("/api/"), Some(true)),
        ];
        let toml_text = build_pingap_config(&projects).expect("proxied → Some");
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

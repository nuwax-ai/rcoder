//! UserApp Deployment 纯辅助函数(从 k8s_deployment.rs 拆出)。
//!
//! port-expose 注解编解码 + config_hash 注解 + probe 构建。create/query 共用。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerCreateParams, ExposeType};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::Probe;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;

pub(crate) const PORT_EXPOSE_ANNOTATION: &str = "rcoder.io/port-expose";


/// ports → annotation（`rcoder.io/port-expose: "80:http,5432:tcp"`）；无端口返 None。
#[cfg(feature = "kubernetes")]
pub(crate) fn encode_port_expose_annotations(
    params: &ContainerCreateParams,
) -> Option<BTreeMap<String, String>> {
    let ports = params.ports.as_ref()?;
    if ports.is_empty() {
        return None;
    }
    // 按 port 排序后编码——避免调用方端口顺序差异触发 SSA 无谓 reconcile（顺序无关 → 字符串稳定）
    let mut entries: Vec<(u16, &ExposeType)> =
        ports.iter().map(|p| (p.port, &p.expose_type)).collect();
    entries.sort_by_key(|(port, _)| *port);
    let val = entries
        .iter()
        .map(|(port, et)| format!("{}:{}", port, expose_type_str(et)))
        .collect::<Vec<_>>()
        .join(",");
    let mut m = BTreeMap::new();
    m.insert(PORT_EXPOSE_ANNOTATION.to_string(), val);
    Some(m)
}

/// 解析 "port:type,..." → port→expose_type 映射（容错：非法条目跳过）。
#[cfg(feature = "kubernetes")]
pub(crate) fn parse_port_expose(s: &str) -> std::collections::HashMap<u16, ExposeType> {
    s.split(',')
        .filter_map(|entry| {
            let mut it = entry.split(':');
            let port: u16 = it.next()?.trim().parse().ok()?;
            let et = match it.next()?.trim() {
                "tcp" => ExposeType::Tcp,
                _ => ExposeType::Http,
            };
            Some((port, et))
        })
        .collect()
}

#[cfg(feature = "kubernetes")]
pub(crate) fn expose_type_str(e: &ExposeType) -> &'static str {
    match e {
        ExposeType::Http => "http",
        ExposeType::Tcp => "tcp",
    }
}

/// 计算 env+secrets 内容的 hash，注入 pod template annotation。
///
/// 作用：env/secrets 走 ConfigMap/Secret，改内容时 `env_from` 引用名不变 → Deployment spec
/// 不变 → 不触发 rollout → 新 env 到不了运行中 Pod。此 hash 进 pod template，内容变即
/// annotation 变 → spec 变 → 自动 rollout。DefaultHasher 跨进程确定（固定 key），故同
/// 内容多次 apply 的 hash 稳定，不会引发误 rollout。
#[cfg(feature = "kubernetes")]
pub(crate) fn config_hash_annotations(
    params: &container_runtime_api::ContainerCreateParams,
) -> BTreeMap<String, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for map in [params.env.as_ref(), params.secrets.as_ref()]
        .into_iter()
        .flatten()
    {
        let mut items: Vec<_> = map.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in items {
            k.hash(&mut h);
            v.hash(&mut h);
        }
    }
    let mut ann = BTreeMap::new();
    ann.insert(
        "rcoder.io/config-hash".to_string(),
        format!("{:016x}", h.finish()),
    );
    ann
}

/// 健康检查配置 → K8s Probe
#[cfg(feature = "kubernetes")]
pub(crate) fn build_probe(hc: &container_runtime_api::AppHealthCheck) -> Option<Probe> {
    use container_runtime_api::HealthCheckType;
    let init = hc.initial_delay_seconds.map(|s| s as i32);
    let period = hc.period_seconds.map(|s| s as i32);
    match hc.check_type {
        HealthCheckType::None | HealthCheckType::Exec => None,
        HealthCheckType::Http => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                    path: Some(hc.path.clone().unwrap_or_else(|| "/".to_string())),
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
        HealthCheckType::Tcp => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                tcp_socket: Some(k8s_openapi::api::core::v1::TCPSocketAction {
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
    }
}
